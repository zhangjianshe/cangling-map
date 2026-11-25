#!/usr/bin/env python3
"""Isolated 3D Tiles publication: snapshots, URI safety, HTTP semantics and restart."""
import json,os,sqlite3,socket,subprocess,tempfile,time,struct,copy
from pathlib import Path
from urllib.request import Request,urlopen
from urllib.error import HTTPError
BINARY=Path('target/debug/map-server').resolve()
def make_glb():
    binary=struct.pack('<9f',0,0,0,10,0,0,0,10,0)
    doc={'asset':{'version':'2.0'},'buffers':[{'byteLength':len(binary)}],'bufferViews':[{'buffer':0,'byteLength':len(binary)}],'accessors':[{'bufferView':0,'componentType':5126,'count':3,'type':'VEC3','min':[0,0,0],'max':[10,10,0]}],'meshes':[{'primitives':[{'attributes':{'POSITION':0},'material':0}]}],'materials':[{'doubleSided':True}],'nodes':[{'mesh':0}],'scenes':[{'nodes':[0]}],'scene':0}
    raw=json.dumps(doc).encode();raw+=b' '*(-len(raw)%4)
    return b'glTF'+struct.pack('<II',2,28+len(raw)+len(binary))+struct.pack('<I4s',len(raw),b'JSON')+raw+struct.pack('<I4s',len(binary),b'BIN\0')+binary
with tempfile.TemporaryDirectory(prefix='map-3dtiles-') as temp:
    root=Path(temp);source=root/'data/demo/source';source.mkdir(parents=True)
    glb=make_glb();(source/'model.glb').write_bytes(glb)
    tile={'asset':{'version':'1.1'},'geometricError':100,'root':{'boundingVolume':{'sphere':[0,0,0,100]},'geometricError':0,'refine':'ADD','content':{'uri':'model.glb'},'transform':[1,0,0,0,0,1,0,0,0,0,1,0,6378137,0,0,1]}}
    (source/'tileset.json').write_text(json.dumps(tile));(source/'private.txt').write_text('not published')
    with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
    base=f'http://127.0.0.1:{port}'
    with sqlite3.connect(root/'places.db') as db:db.execute('CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT)');db.execute("INSERT INTO metadata VALUES('schema_version','1')")
    env={**os.environ,'MAP_SERVER_ADDRESS':f'127.0.0.1:{port}','CATALOG_DATABASE_URL':f'sqlite://{root}/catalog.db?mode=rwc','IMAGERY_DATABASE_URL':f'sqlite://{root}/imagery.db?mode=rwc','IMAGERY_INDEX_DATABASE_URL':f'sqlite://{root}/index.db?mode=rwc','PLACENAMES_DATABASE_URL':f'sqlite://{root}/places.db?mode=ro','WMTS_CACHE_BACKEND':'filesystem','WMTS_CACHE_PATH':str(root/'cache'),'TILES3D_DATA_ROOT':str(root/'data')}
    log=open(root/'server.log','w');proc=None
    def req(path,method='GET',body=None,headers=None,status=200):
        try:r=urlopen(Request(base+path,data=None if body is None else json.dumps(body).encode(),method=method,headers={'Content-Type':'application/json',**(headers or {})}),timeout=20)
        except HTTPError as e:r=e
        with r:
            data=r.read();assert r.status==status,(path,r.status,data.decode(errors='replace'));return data,r.headers,r.geturl()
    def api(path,method='GET',body=None,status=200,headers=None):return json.loads(req(path,method,body,headers,status)[0])
    def start():
        global proc
        proc=subprocess.Popen([str(BINARY)],cwd=root,env=env,stdout=log,stderr=log)
        for _ in range(100):
            try:req('/health');return
            except OSError:
                if proc.poll() is not None:raise AssertionError((root/'server.log').read_text())
                time.sleep(.1)
        raise AssertionError('startup')
    try:
        start();api('/api/3dtiles/demo','PUT',{'title':'三维测试'})
        req('/api/3dtiles/demo','PUT',{'title':'bad'},headers={'Sec-Fetch-Site':'cross-site'},status=403)
        manifest=api('/api/3dtiles/demo/validate','POST');assert manifest['tile_count']==1 and len(manifest['files'])==2
        publication=api('/api/3dtiles/demo/publish','POST');revision=publication['manifest']['revision'];prefix='/3dtiles/demo/versions/'+revision
        body,headers,resolved=req('/3dtiles/demo/tileset.json');assert json.loads(body)['root']['content']['uri']==f'versions/{revision}/model.glb' and resolved.endswith('/3dtiles/demo/tileset.json')
        body,headers,_=req(prefix+'/model.glb',headers={'Origin':'http://localhost:4301'});assert body==glb and headers['Access-Control-Allow-Origin']=='*' and headers['Content-Type']=='model/gltf-binary'
        req(prefix+'/model.glb',headers={'If-None-Match':headers['ETag']},status=304)
        body,headers,_=req(prefix+'/model.glb',headers={'Range':'bytes=0-11'},status=206);assert body==glb[:12]
        body,headers,_=req(prefix+'/model.glb','HEAD');assert not body and int(headers['Content-Length'])==len(glb)
        req(prefix+'/private.txt',status=404)
        req(prefix+'/%2e%2e/source/tileset.json',status=400)
        # A failed replacement must preserve the active version.
        for uri in ['../secret.glb','https://example.org/m.glb','/etc/passwd','missing.glb','model%2eglb']:
            bad=copy.deepcopy(tile);bad['root']['content']['uri']=uri;(source/'tileset.json').write_text(json.dumps(bad));req('/api/3dtiles/demo/publish','POST',status=400);assert api('/api/3dtiles/demo')['dataset']['revision']==revision
        (source/'link.glb').symlink_to(source/'model.glb');bad['root']['content']['uri']='link.glb';(source/'tileset.json').write_text(json.dumps(bad));req('/api/3dtiles/demo/validate','POST',status=400)
        bad=copy.deepcopy(tile);bad['root']['content']['uri']='tileset.json';(source/'tileset.json').write_text(json.dumps(bad));req('/api/3dtiles/demo/validate','POST',status=400)
        # Legacy b3dm/pnts headers and bounded feature tables.
        for extension,feature,binary in [('b3dm',{'BATCH_LENGTH':0},b''),('pnts',{'POINTS_LENGTH':1,'POSITION':{'byteOffset':0}},struct.pack('<3f',0,0,0))]:
            raw=json.dumps(feature).encode();raw+=b' '*((-(28+len(raw)))%8)
            payload=glb if extension=='b3dm' else b''
            content=extension.encode()+struct.pack('<6I',1,28+len(raw)+len(binary)+len(payload),len(raw),len(binary),0,0)+raw+binary+payload
            (source/('legacy.'+extension)).write_bytes(content)
            legacy=copy.deepcopy(tile);legacy['asset']['version']='1.0';legacy['root']['content']['uri']='legacy.'+extension;(source/'tileset.json').write_text(json.dumps(legacy))
            api('/api/3dtiles/demo/validate','POST')
            (source/('legacy.'+extension)).write_bytes(content[:-1]);req('/api/3dtiles/demo/validate','POST',status=400)
        # Nested external tilesets retain their own relative base.
        nested=source/'nested';nested.mkdir();(nested/'model.glb').write_bytes(glb);nested_tile=copy.deepcopy(tile);nested_tile['root'].pop('transform');(nested/'tileset.json').write_text(json.dumps(nested_tile))
        new=copy.deepcopy(tile);new['root']['content']['uri']='nested/tileset.json';(source/'tileset.json').write_text(json.dumps(new))
        published=api('/api/3dtiles/demo/publish','POST');rev2=published['manifest']['revision'];assert rev2!=revision
        assert req('/3dtiles/demo/versions/'+rev2+'/nested/model.glb')[0]==glb
        # Version snapshots survive source edits and restart; previous versions stay available.
        (source/'model.glb').write_bytes(b'changed');assert req(prefix+'/model.glb')[0]==glb
        proc.terminate();proc.wait(10);start();assert api('/api/3dtiles/demo')['dataset']['revision']==rev2
        assert req(prefix+'/model.glb')[0]==glb
        if os.environ.get('TILES3D_BROWSER_TEST'):
            api('/api/3dtiles/demo','PUT',{'title':'3D Tiles 验证数据集'})
            print('BROWSER_BASE='+base,flush=True)
            Path('/tmp/cangling-tiles3d-browser-base').write_text(base)
            input('Press Enter to complete verification and clean up\n')
        api('/api/3dtiles/demo/disable','POST');req('/3dtiles/demo/tileset.json',status=404);req(prefix+'/model.glb',status=404)
        body=req('/3dtiles',headers={'X-Forwarded-Prefix':'/map'})[0].decode();assert '/map/assets/tiles3d.js' in body and '/map/3dtiles' in body
        print('PASS 3D Tiles: validation, publication, nested references, immutable versions, Range/HEAD/ETag/CORS, disable, restart, traversal/symlink rejection')
    finally:
        if proc and proc.poll() is None:proc.terminate();proc.wait(10)
        log.close()
