#!/usr/bin/env python3
"""Isolated real HTTP read-through regression; no production catalogs are modified."""
import argparse, hashlib, json, os, pathlib, shutil, socket, sqlite3, subprocess, tempfile, time, urllib.request, urllib.error
p=argparse.ArgumentParser();p.add_argument('--map-binary',required=True);p.add_argument('--tile-binary',required=True);p.add_argument('--source-root',required=True);a=p.parse_args()
def port():
 with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]
def request(url,method='GET',data=None,headers=None):
 r=urllib.request.Request(url,data=None if data is None else json.dumps(data).encode(),method=method,headers={'Content-Type':'application/json',**(headers or {})})
 try:
  with urllib.request.urlopen(r,timeout=15) as v:return v.status,dict(v.headers),v.read()
 except urllib.error.HTTPError as e:return e.code,dict(e.headers),e.read()
def ready(url,proc):
 for _ in range(100):
  if proc.poll() is not None:raise RuntimeError('service exited')
  try:
   if request(url+'/health')[0]==200:return
  except OSError:pass
  time.sleep(.1)
 raise RuntimeError('health timeout')
with tempfile.TemporaryDirectory(prefix='tiles3d-regression-') as tmp:
 root=pathlib.Path(tmp);mp='http://127.0.0.1:'+str(port());tp='http://127.0.0.1:'+str(port());token='isolated-regression-token'
 env={**os.environ,'TILES3D_CACHE_URL':tp,'TILES3D_CACHE_TOKEN':token,'TILES3D_DATA_ROOT':str(root/'datasets'),'MAP_SERVER_ADDRESS':mp[7:],'PLACENAMES_DATABASE_URL':'sqlite://places.db?mode=rwc','MAP_RENDER_BACKEND':'cpu','WMTS_CACHE_BACKEND':'filesystem','CANGLING_TILE_AUTH_TOKEN':token,'CANGLING_TILE_ADDR':tp[7:],'CANGLING_TILE_ROOT':str(root/'cache'),'CANGLING_TILE_CONFIG_DIR':str(root/'config'),'CANGLING_TILE_MEMORY_CACHE_BYTES':'0'}
 with sqlite3.connect(root/"places.db") as db:
  db.executescript("CREATE TABLE metadata(key TEXT,value TEXT); INSERT INTO metadata VALUES('schema_version','1');")
 procs=[]
 try:
  tile=subprocess.Popen([a.tile_binary],cwd=root,env=env,stdout=open(root/'tile.log','w'),stderr=subprocess.STDOUT);procs.append(tile);ready(tp,tile)
  mp_proc=subprocess.Popen([a.map_binary],cwd=root,env=env,stdout=open(root/'map.log','w'),stderr=subprocess.STDOUT);procs.append(mp_proc);ready(mp,mp_proc)
  samples=[];count=0
  for dataset in ['stanford-dragon','zhengzhou-cbd-osm']:
   shutil.copytree(pathlib.Path(a.source_root)/dataset/'source',root/'datasets'/dataset/'source')
   assert request(mp+'/api/3dtiles/'+dataset,'PUT',{'title':dataset})[0]==200
   code,_,data=request(mp+'/api/3dtiles/'+dataset+'/publish','POST',{});assert code==200,data
   manifest=json.loads(data)['manifest'];rev=manifest['revision']
   for path,info in manifest['files'].items():
    url=f'{mp}/3dtiles/{dataset}/versions/{rev}/{urllib.parse.quote(path)}'
    for expected in ['MISS','HIT']:
     code,headers,data=request(url);assert code==200,(url,code,data)
     assert headers['x-tiles3d-cache']==expected,(url,headers)
     assert hashlib.sha256(data).hexdigest()==info['sha256']
    samples.append((url,data));count+=1
   assert request(mp+f'/3dtiles/{dataset}/tileset.json')[1]['x-tiles3d-cache']=='HIT'
  url,data=samples[-1]
  code,h,b=request(url,headers={'Range':'bytes=1-15'});assert code==206 and b==data[1:16]
  assert request(url,method='HEAD')[2]==b''
  assert request(url,headers={'If-None-Match':h['etag']})[0]==304
  assert request(tp+'/api/v1/objects3d/stats')[0]==401
  code,h,b=request(tp+'/api/v1/objects3d/stats',headers={'Authorization':'Bearer '+token});stats=json.loads(b);assert code==200
  assert sum(v['objects'] for v in stats['datasets'])==count
  assert request(mp+'/api/3dtiles/zhengzhou-cbd-osm/disable','POST',{})[0]==200
  assert request(url)[0]==404 # persisted objects cannot bypass visibility
  tile.terminate();tile.wait(timeout=10)
  code,h,b=request(samples[0][0]);assert code==200 and b==samples[0][1] and h['x-tiles3d-cache']=='BYPASS'
  print(json.dumps({'verified_files':count,'miss_hit_sha256':True,'range_head_etag':True,'disabled_dataset':True,'tile_failure_fallback':True,'cache_stats':stats},indent=2))
 except Exception:
  for log in root.glob('*.log'):print(log.name,log.read_text()[-3000:])
  raise
 finally:
  for proc in procs:
   if proc.poll() is None:proc.terminate();proc.wait(timeout=10)
