#!/usr/bin/env python3
"""Validate generated Zhengzhou LOD geometry, metadata, containment and resource budgets."""
import argparse,json,struct,math
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--data-root',type=Path,required=True);root=p.parse_args().data_root
source=root/'source';tiles=json.loads((source/'tileset.json').read_text());report=json.loads((root/'report.json').read_text());features=json.loads((root/'buildings.geojson').read_text())['features']
expected={f['id'] for f in features};counts={};ids={};files=[]
def bounds(b):
 c=b[:3];h=[sum(abs(b[3+j*3+i]) for j in range(3)) for i in range(3)]
 return [c[i]-h[i] for i in range(3)],[c[i]+h[i] for i in range(3)]
def walk(node,parent=None):
 lo,hi=bounds(node['boundingVolume']['box'])
 if parent:
  a,b=bounds(parent['boundingVolume']['box']);assert all(a[i]-1e-6<=lo[i]<=hi[i]<=b[i]+1e-6 for i in range(3))
  assert parent['geometricError']>=node['geometricError']
 level=node['extras']['lod'];uri=node['content']['uri'];files.append(uri)
 raw=(source/uri).read_bytes();magic,version,size=struct.unpack_from('<4sII',raw);assert (magic,version,size)==(b'glTF',2,len(raw))
 length,kind=struct.unpack_from('<I4s',raw,12);assert kind==b'JSON';g=json.loads(raw[20:20+length]);length2,kind2=struct.unpack_from('<I4s',raw,20+length);assert kind2==b'BIN\0';binary=raw[28+length:];assert len(binary)==length2
 primitive=g['meshes'][0]['primitives'][0];pos=g['accessors'][primitive['attributes']['POSITION']];norm=g['accessors'][primitive['attributes']['NORMAL']];assert pos['count']==norm['count'] and pos['count']%3==0
 for a,is_normal in [(pos,False),(norm,True)]:
  view=g['bufferViews'][a['bufferView']];offset=view.get('byteOffset',0)+a.get('byteOffset',0)
  for i in range(a['count']):
   v=struct.unpack_from('<3f',binary,offset+12*i);assert all(math.isfinite(x) for x in v)
   if is_normal:assert abs(sum(x*x for x in v)-1)<1e-5
   else:
    enu=[v[0],-v[2],v[1]];assert all(lo[j]-1e-3<=enu[j]<=hi[j]+1e-3 for j in range(3))
 if level=='high':
  assert len(g['meshes'][0]['primitives'])==2
  assert g['samplers'][0]['wrapS']==10497 and g['samplers'][0]['wrapT']==10497
  image=g['bufferViews'][g['images'][0]['bufferView']];assert binary[image['byteOffset']:image['byteOffset']+8]==b'\x89PNG\r\n\x1a\n'
  referenced=[]
  for part in g['meshes'][0]['primitives']:
   a=g['accessors'][part['indices']];v=g['bufferViews'][a['bufferView']]
   referenced.extend(struct.unpack_from('<'+'I'*a['count'],binary,v['byteOffset']))
  assert sorted(referenced)==list(range(pos['count']))
  uv=g['accessors'][primitive['attributes']['TEXCOORD_0']];v=g['bufferViews'][uv['bufferView']]
  assert uv['count']==pos['count'];assert all(math.isfinite(x) for x in struct.unpack_from('<'+'f'*uv['count']*2,binary,v['byteOffset']))
 else:assert 'images' not in g
 counts[level]=counts.get(level,0)+pos['count'];ids.setdefault(level,set()).update(g['extras']['osmIds'])
 for child in node.get('children',[]):
  assert node['refine']=='REPLACE';walk(child,node)
walk(tiles['root'])
assert len(files)==len(set(files))==30
assert counts['low']<counts['medium']<counts['high']
for level in counts:
 assert ids[level]==expected
 assert counts[level]==report['lod'][level]['vertices']
 assert report['lod'][level]['measuredErrorMetres']<=report['lod'][level]['geometricError']
print(f'PASS: 3 levels, {len(expected)} IDs preserved, finite unit normals, roof/geometry counts, bounds containment, 30 resources, REPLACE tree')
print(counts)
