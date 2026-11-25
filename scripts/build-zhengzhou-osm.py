#!/usr/bin/env python3
"""OSM map XML -> clipped 3km square -> terrain-aligned flat-roof GLB tiles.
Dependencies: numpy shapely pyproj mapbox-earcut pillow. Raw inputs are cached.
This is an OSM-derived preview, not a surveyed building model.
"""
import json, math, struct, re, hashlib, collections, argparse, io
import xml.etree.ElementTree as ET
from pathlib import Path
from urllib.request import urlopen
import numpy as np
import mapbox_earcut
from PIL import Image
from pyproj import CRS, Transformer
from shapely.geometry import Polygon, LineString, box, mapping
from shapely.ops import polygonize, unary_union, transform
from shapely import make_valid
from shapely.geometry.polygon import orient
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--data-root',type=Path,required=True,help='Directory containing map.osm; outputs source/tileset.json')
ROOT=parser.parse_args().data_root
root=ET.parse(ROOT/'map.osm').getroot()
nodes={int(n.attrib['id']):{'lon':float(n.attrib['lon']),'lat':float(n.attrib['lat'])} for n in root.findall('node')}; ways={};out=[]
for w in root.findall('way'):
 refs=[int(n.attrib['ref']) for n in w.findall('nd')]; tags={n.attrib['k']:n.attrib['v'] for n in w.findall('tag')}
 ways[int(w.attrib['id'])]={'type':'way','id':int(w.attrib['id']),'tags':tags,'geometry':[nodes[r] for r in refs if r in nodes]}
for w in ways.values():
 if 'building' in w['tags'] or 'building:part' in w['tags']:out.append(w)
missing=[]
for r in root.findall('relation'):
 tags={n.attrib['k']:n.attrib['v'] for n in r.findall('tag')}
 if not ('building' in tags or 'building:part' in tags):continue
 members=[]
 for m in r.findall('member'):
  ref=int(m.attrib['ref']);v={'type':m.attrib['type'],'ref':ref,'role':m.attrib.get('role','')}
  if v['type']=='way':
   if ref not in ways:missing.append(ref)
   else:v['geometry']=ways[ref]['geometry']
  members.append(v)
 out.append({'type':'relation','id':int(r.attrib['id']),'tags':tags,'members':members})
assert not missing,missing
(ROOT/'osm-buildings.json').write_text(json.dumps({'generator':'OSM API XML converted to out-geom JSON','osm3s':{'source':'https://api.openstreetmap.org/api/0.6/map?bbox=113.7109,34.7610,113.7451,34.7890','copyright':'© OpenStreetMap contributors; ODbL 1.0'},'elements':out},ensure_ascii=False))

LON,LAT=113.728,34.775
local=CRS.from_proj4(f'+proj=aeqd +lat_0={LAT} +lon_0={LON} +datum=WGS84 +units=m')
fwd=Transformer.from_crs(4326,local,always_xy=True).transform
inv=Transformer.from_crs(local,4326,always_xy=True).transform
ecef=Transformer.from_crs(4979,4978,always_xy=True)
origin=np.array(ecef.transform(LON,LAT,0))
lo,la=map(math.radians,(LON,LAT))
E=np.array([-math.sin(lo),math.cos(lo),0]);N=np.array([-math.sin(la)*math.cos(lo),-math.sin(la)*math.sin(lo),math.cos(la)]);U=np.array([math.cos(la)*math.cos(lo),math.cos(la)*math.sin(lo),math.sin(la)])
rotation=np.column_stack([E,N,U]);world=np.eye(4);world[:3,:3]=rotation;world[:3,3]=origin
area=box(-1500,-1500,1500,1500)
raw=json.loads((ROOT/'osm-buildings.json').read_text());assert not raw.get('remark'),raw.get('remark')
cache=ROOT/'terrain';cache.mkdir(exist_ok=True)
meta_path=cache/'metadata.json'
if not meta_path.exists():meta_path.write_bytes(urlopen('https://agri.cangling.cn:22002/api/terrain/china-cop30/metadata',timeout=30).read())
meta=json.loads(meta_path.read_text());images={}
def terrain(lon,lat):
 z=12;n=2**z;px=(lon+180)/360*n*256;py=(1-math.asinh(math.tan(math.radians(lat)))/math.pi)/2*n*256
 x,y=int(px//256),int(py//256);key=(x,y)
 if key not in images:
  p=cache/f'{z}-{x}-{y}.png'
  if not p.exists():p.write_bytes(urlopen('https://agri.cangling.cn:22002'+meta['tileUrl'].format(z=z,x=x,y=y),timeout=40).read())
  images[key]=np.asarray(Image.open(p).convert('RGB')).astype(float)
 rgb=images[key][min(255,int(py%256)),min(255,int(px%256))]
 assert rgb.any(),'No terrain value';return rgb[0]*256+rgb[1]+rgb[2]/256-32768

def polygons(g):
 if g.is_empty:return []
 if g.geom_type=='Polygon':return [g]
 return [p for sub in getattr(g,'geoms',[]) for p in polygons(sub)]
def geometry(e):
 if e['type']=='way':
  a=[(p['lon'],p['lat']) for p in e.get('geometry',[]) if 'lon' in p]
  if len(a)<4 or a[0]!=a[-1]:return None
  return make_valid(Polygon(a))
 if e['type']=='relation':
  outer=[];inner=[]
  for m in e.get('members',[]):
   a=[(p['lon'],p['lat']) for p in m.get('geometry',[]) if 'lon' in p]
   if len(a)>1:(inner if m.get('role')=='inner' else outer).append(LineString(a))
  shells=unary_union(list(polygonize(outer)));holes=unary_union(list(polygonize(inner)))
  return make_valid(shells.difference(holes))

def number(s):
 try:
  s=str(s).strip().lower();m=re.fullmatch(r'(\d+(?:\.\d+)?)\s*(m|米|ft)?',s)
  return float(m[1])*(.3048 if m[2]=='ft' else 1) if m else None
 except (TypeError,ValueError):return None
features=[];skip=[];members=set()
for e in raw['elements']:
 if e['type']=='relation' and e.get('tags',{}).get('type')=='multipolygon':
  members.update(m['ref'] for m in e.get('members',[]) if m['type']=='way')
for e in raw['elements']:
 tags=e.get('tags',{});oid=f"{e['type']}/{e['id']}"
 if e['type']=='way' and e['id'] in members:continue
 if tags.get('building')=='no' or tags.get('building:part')=='no':continue
 g=geometry(e)
 if g is None or g.is_empty:skip.append({'id':oid,'reason':'invalid/missing polygon'});continue
 g=make_valid(transform(fwd,g)).intersection(area)
 if not polygons(g):continue
 levels=number(tags.get('building:levels'));height=number(tags.get('height'));source='height'
 if not height:
  height=levels*3 if levels else 9;source='levels_x_3m' if levels else 'default_9m'
 if not 0<height<=1000:skip.append({'id':oid,'reason':'height out of range'});continue
 bottom=number(tags.get('min_height')) or ((number(tags.get('building:min_level')) or 0)*3)
 if bottom>=height:skip.append({'id':oid,'reason':'min_height >= height'});continue
 features.append({'id':oid,'tags':tags,'geometry':g,'height':height,'min_height':bottom,'height_source':source,'levels':levels})
# A detailed part supersedes the same area of its building outline.
parts=unary_union([f['geometry'] for f in features if f['tags'].get('building:part') not in (None,'no')])
if not parts.is_empty:
 for f in features:
  if not f['tags'].get('building:part'):f['geometry']=f['geometry'].difference(parts)
features=[f for f in features if polygons(f['geometry'])]
source_dir=ROOT/'source';source_dir.mkdir(exist_ok=True)
tiles=collections.defaultdict(list);out_features=[]
for f in features:
 g=f['geometry']; samples=[g.representative_point()]
 from shapely.geometry import Point
 for p in polygons(g):samples.extend(Point(x,y) for x,y in list(p.exterior.coords)[::max(1,len(p.exterior.coords)//12)])
 ground=max(terrain(*inv(p.x,p.y)) for p in samples)+.5
 f['ground']=ground
 c=g.centroid;key=(min(5,max(0,int((c.x+1500)//500))),min(5,max(0,int((c.y+1500)//500))))
 tiles[key].append(f)
 out_features.append({'type':'Feature','id':f['id'],'geometry':mapping(transform(inv,g)),'properties':{k:v for k,v in f.items() if k!='geometry'}})

def cross2(a,b):return a[0]*b[1]-a[1]*b[0]
# Original procedural facade; no photographed facade or third-party texture.
facade=Image.new('RGBA',(64,64),(210,195,170,255))
from PIL import ImageDraw
pen=ImageDraw.Draw(facade);pen.rectangle((13,11,51,50),fill=(55,120,170,255))
pen.rectangle((30,11,33,50),fill=(235,240,245,255));pen.rectangle((13,29,51,32),fill=(235,240,245,255))
facade_bytes=io.BytesIO();facade.save(facade_bytes,format='PNG');facade_png=facade_bytes.getvalue()
# Shallow procedural window-frame relief; no geometry or building height changes.
relief=np.zeros((64,64),dtype=float);relief[10:52,12:53]=.25;relief[13:49,15:50]=-.1
nx=-(np.roll(relief,-1,axis=1)-np.roll(relief,1,axis=1));ny=-(np.roll(relief,-1,axis=0)-np.roll(relief,1,axis=0));nz=np.ones_like(nx)
normal=np.stack([nx,ny,nz],axis=2);normal/=np.linalg.norm(normal,axis=2,keepdims=True)
normal_image=Image.fromarray(np.round((normal*.5+.5)*255).astype('uint8'),'RGB')
normal_bytes=io.BytesIO();normal_image.save(normal_bytes,format='PNG');normal_png=normal_bytes.getvalue()
material_image=Image.new('RGB',(64,64),(255,225,255));ImageDraw.Draw(material_image).rectangle((13,11,51,50),fill=(255,55,255))
material_bytes=io.BytesIO();material_image.save(material_bytes,format='PNG');material_png=material_bytes.getvalue()

def glb(fs,textured=True):
 positions=[];normals=[];uvs=[];wall_indices=[];roof_indices=[]
 def xyz(x,y,z):
  lon,lat=inv(x,y);v=rotation.T@(np.array(ecef.transform(lon,lat,z))-origin)
  return np.array([v[0],v[2],-v[1]]) # glTF Y-up -> tile ENU Z-up in renderer
 def triangle(a,b,c,uv=None):
  n=np.cross(b-a,c-a);length=np.linalg.norm(n)
  if length<1e-8:return
  n/=length
  (wall_indices if uv else roof_indices).extend(range(len(positions),len(positions)+3))
  positions.extend([a,b,c]);normals.extend([n,n,n]);uvs.extend(uv or [(0.,0.)]*3)
 for f in fs:
  for p in polygons(f['geometry']):
   p=orient(p,sign=1);rings=[np.asarray(p.exterior.coords[:-1])]+[np.asarray(r.coords[:-1]) for r in p.interiors]
   xy=np.concatenate(rings);ends=np.cumsum([len(r) for r in rings],dtype=np.uint32)
   indices=mapbox_earcut.triangulate_float64(xy.astype(np.float64),ends).reshape(-1,3)
   tri_area=sum(abs(float(cross2(xy[b]-xy[a],xy[c]-xy[a])))/2 for a,b,c in indices)
   assert abs(tri_area-p.area)<max(.01,p.area*1e-6), 'Roof triangulation area mismatch'
   top=f['ground']+f['height'];base=f['ground']+f['min_height']
   roof=[xyz(x,y,top) for x,y in xy]
   for a,b,c in indices:
    # Earcut winding checked in local plane; upward-facing roof.
    if cross2(xy[b]-xy[a],xy[c]-xy[a])<0:b,c=c,b
    triangle(roof[a],roof[b],roof[c])
   for ring in rings:
    for a,b in zip(ring,np.roll(ring,-1,axis=0)):
     aa,bb=xyz(*a,base),xyz(*b,base);at,bt=xyz(*a,top),xyz(*b,top)
     width=float(np.linalg.norm(b-a))/3.
     floors=max(1.,(f['height']-f['min_height'])/3.)
     # glTF image V points down; level height is estimated using the same 3 m rule.
     triangle(aa,bb,bt,[(0.,floors),(width,floors),(width,0.)]);triangle(aa,bt,at,[(0.,floors),(width,0.),(0.,0.)])
 pos=np.array(positions,dtype='<f4');norm=np.array(normals,dtype='<f4');assert np.isfinite(pos).all() and np.isfinite(norm).all()
 assert len(pos)<500000
 binary=pos.tobytes()+norm.tobytes()
 doc={'asset':{'version':'2.0','copyright':'© OpenStreetMap contributors; ODbL 1.0','generator':'Cangling OSM preview'},'buffers':[{'byteLength':len(binary)}],'bufferViews':[{'buffer':0,'byteOffset':0,'byteLength':pos.nbytes},{'buffer':0,'byteOffset':pos.nbytes,'byteLength':norm.nbytes}],'accessors':[{'bufferView':0,'componentType':5126,'count':len(pos),'type':'VEC3','min':pos.min(axis=0).tolist(),'max':pos.max(axis=0).tolist()},{'bufferView':1,'componentType':5126,'count':len(norm),'type':'VEC3'}],'materials':[{'doubleSided':True,'pbrMetallicRoughness':{'baseColorFactor':[.8,.83,.86,1],'metallicFactor':0,'roughnessFactor':.9}}],'meshes':[{'primitives':[{'attributes':{'POSITION':0,'NORMAL':1},'material':0}]}],'nodes':[{'mesh':0}],'scenes':[{'nodes':[0]}],'scene':0,'extras':{'osmIds':[f['id'] for f in fs]}}
 if textured:
  binary=bytearray(binary)
  def append(raw):
   offset=len(binary);binary.extend(raw);binary.extend(b'\0'*(-len(binary)%4))
   doc['bufferViews'].append({'buffer':0,'byteOffset':offset,'byteLength':len(raw)})
   return len(doc['bufferViews'])-1
  uv=np.asarray(uvs,dtype='<f4');assert np.isfinite(uv).all()
  doc['accessors'].append({'bufferView':append(uv.tobytes()),'componentType':5126,'count':len(uv),'type':'VEC2'})
  doc['images']=[{'bufferView':append(raw),'mimeType':'image/png'} for raw in (facade_png,normal_png,material_png)]
  doc['samplers']=[{'wrapS':10497,'wrapT':10497,'magFilter':9729,'minFilter':9987}]
  doc['textures']=[{'source':i,'sampler':0} for i in range(3)]
  doc['materials']=[{'doubleSided':True,'pbrMetallicRoughness':{'baseColorFactor':[.4,.43,.46,1],'metallicFactor':0,'roughnessFactor':1}}, {'doubleSided':True,'pbrMetallicRoughness':{'baseColorTexture':{'index':0},'metallicFactor':0,'roughnessFactor':.9}}]
  doc['materials'][1]['normalTexture']={'index':1,'scale':.5}
  doc['materials'][1]['pbrMetallicRoughness']['metallicRoughnessTexture']={'index':2}
  primitives=[]
  for material,indices in [(0,roof_indices),(1,wall_indices)]:
   view=append(np.asarray(indices,dtype='<u4').tobytes());accessor=len(doc['accessors'])
   doc['accessors'].append({'bufferView':view,'componentType':5125,'count':len(indices),'type':'SCALAR'})
   primitives.append({'attributes':{'POSITION':0,'NORMAL':1,'TEXCOORD_0':2},'indices':accessor,'material':material})
  doc['meshes'][0]['primitives']=primitives;doc['buffers'][0]['byteLength']=len(binary)
  doc['extras']['appearance']='procedural windows, 3 m repeat; not surveyed facade'
  binary=bytes(binary)
 raw=json.dumps(doc,separators=(',',':')).encode();raw+=b' '*(-len(raw)%4)
 data=b'glTF'+struct.pack('<II',2,28+len(raw)+len(binary))+struct.pack('<I4s',len(raw),b'JSON')+raw+struct.pack('<I4s',len(binary),b'BIN\0')+binary
 # Bounding volume in tile ENU, before glTF Y-up conversion.
 enu=pos[:,[0,2,1]].copy();enu[:,1]*=-1
 low,high=enu.min(axis=0)-.1,enu.max(axis=0)+.1;c=(low+high)/2;half=(high-low)/2
 bv={'box':list(map(float,[*c,half[0],0,0,0,half[1],0,0,0,half[2]]))}
 return data,bv,len(pos)
# Coarse whole-district parent -> simplified whole-district parent -> 28 full-detail
# spatial leaves. At most 30 geometry resources are needed, below Earth's 32 budget.
# REPLACE keeps a ready parent visible while its children are being fetched.
def simplify_features(fs, tolerance):
 result=[]; max_error=0.
 for f in fs:
  original=f['geometry']
  simplified=original.simplify(tolerance,preserve_topology=True)
  error=original.boundary.hausdorff_distance(simplified.boundary)
  if simplified.is_empty or not simplified.is_valid or error>tolerance*1.001:
   simplified=original;error=0.
  assert len(polygons(simplified))==len(polygons(original))
  assert sum(len(p.interiors) for p in polygons(simplified))==sum(len(p.interiors) for p in polygons(original))
  result.append({**f,'geometry':simplified});max_error=max(max_error,error)
 return result,max_error

children=[];vertices=0;lod_stats={}
for (x,y),fs in sorted(tiles.items()):
 data,bv,count=glb(fs);name=f'buildings-{x}-{y}.glb';(source_dir/name).write_bytes(data);vertices+=count
 children.append({'boundingVolume':bv,'geometricError':0,'content':{'uri':name},'extras':{'lod':'high'}})
assert children,'No buildings found'
lod_stats['high']={'vertices':vertices,'files':len(children),'toleranceMetres':0,'measuredErrorMetres':0,'geometricError':0}
coarse={}
for level,tolerance,error_budget in [('low',20.,40.),('medium',3.,6.)]:
 fs,error=simplify_features(features,tolerance)
 data,bv,count=glb(fs,textured=True);name=f'buildings-{level}.glb';(source_dir/name).write_bytes(data)
 coarse[level]={'boundingVolume':bv,'geometricError':error_budget,'refine':'REPLACE','content':{'uri':name},'extras':{'lod':level}}
 lod_stats[level]={'vertices':count,'files':1,'bytes':len(data),'toleranceMetres':tolerance,'measuredErrorMetres':error,'geometricError':error_budget}
 assert error<=error_budget and {f['id'] for f in fs}=={f['id'] for f in features}
assert lod_stats['low']['vertices']<lod_stats['medium']['vertices']<vertices, 'LOD must reduce actual geometry'
# All ancestors enclose the union of every level, including the leaf bounds.
def union_bounds(bounds):
 lows=[];highs=[]
 for bv in bounds:
  b=np.asarray(bv['box']);c=b[:3];half=np.abs(b[3:].reshape(3,3)).sum(axis=0)
  lows.append(c-half);highs.append(c+half)
 lo=np.min(lows,axis=0);hi=np.max(highs,axis=0);c=(lo+hi)/2;h=(hi-lo)/2
 return {'box':list(map(float,[*c,h[0],0,0,0,h[1],0,0,0,h[2]]))}
full_bounds=union_bounds([n['boundingVolume'] for n in children]+[n['boundingVolume'] for n in coarse.values()])
coarse['medium']['boundingVolume']=full_bounds
coarse['medium']['children']=children
root=coarse['low'];root['boundingVolume']=full_bounds;root['children']=[coarse['medium']]
root['transform']=world.flatten(order='F').tolist()
counts=dict(collections.Counter(f['height_source'] for f in features))
summary={'name':'郑州郑东新区 CBD 3×3km OSM 建筑','center':[LON,LAT],'localBoundsMetres':[-1500,-1500,1500,1500],'areaKm2':9,'osmTimestamp':json.loads((ROOT/'osm-buildings.json').read_text()).get('osm3s'),'buildings':len(features),'heightSources':counts,'withBuildingLevels':sum(f['levels'] is not None for f in features),'tileCount':len(children)+2,'vertices':vertices,'lod':lod_stats,'skipped':skip,'terrain':meta,'appearance':'All LODs: consistent procedural windows and plain roof; no surveyed facade','heightRule':'height first; building:levels × 3m; missing 9m (estimated); flat roofs','baseRule':'max sampled Copernicus GLO-30 terrain + 0.5m per building; same numeric vertical reference as Earth, not a survey datum conversion','attribution':'© OpenStreetMap contributors','license':'https://www.openstreetmap.org/copyright','rawSha256':hashlib.sha256((ROOT/'osm-buildings.json').read_bytes()).hexdigest()}
(source_dir/'tileset.json').write_text(json.dumps({'asset':{'version':'1.1','copyright':'© OpenStreetMap contributors; ODbL 1.0'},'geometricError':root['geometricError'],'root':root,'extras':{'heightSources':counts,'attribution':'© OpenStreetMap contributors','license':'https://www.openstreetmap.org/copyright','bounds':'3km x 3km local AEQD square; WGS84','lod':lod_stats}},ensure_ascii=False,indent=2))
(ROOT/'buildings.geojson').write_text(json.dumps({'type':'FeatureCollection','features':out_features},ensure_ascii=False))
(ROOT/'report.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2))
print(json.dumps({k:v for k,v in summary.items() if k not in ('terrain','skipped','osmTimestamp')},ensure_ascii=False,indent=2))
