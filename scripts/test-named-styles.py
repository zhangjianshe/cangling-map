#!/usr/bin/env python3
"""Exercise named styles against an isolated local server (cargo build -p map-server first)."""
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import time
from urllib.parse import urlencode
from urllib.request import Request, urlopen
from urllib.error import HTTPError
import xml.etree.ElementTree as ET

BINARY = Path(__file__).resolve().parents[1] / 'target/debug/map-server'
WORLD = '-20037508.342789244,-20037508.342789244,20037508.342789244,20037508.342789244'


def main():
    with tempfile.TemporaryDirectory(prefix='map-named-styles-') as temp:
        root = Path(temp)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        with sqlite3.connect(root / 'places.db') as db:
            db.execute('CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT)')
            db.execute("INSERT INTO metadata VALUES('schema_version','1')")
        env = dict(os.environ, MAP_SERVER_ADDRESS=f'127.0.0.1:{port}',
                   CATALOG_DATABASE_URL=f'sqlite://{root}/catalog.db?mode=rwc',
                   IMAGERY_DATABASE_URL=f'sqlite://{root}/imagery.db?mode=rwc',
                   IMAGERY_INDEX_DATABASE_URL=f'sqlite://{root}/imagery-index.db?mode=rwc',
                   PLACENAMES_DATABASE_URL=f'sqlite://{root}/places.db?mode=ro',
                   WMTS_CACHE_BACKEND='filesystem', WMTS_CACHE_PATH=str(root / 'tiles'))

        def request(path, value=None, method=None):
            data = None if value is None else json.dumps(value).encode()
            req = Request(base + path, data=data, method=method,
                          headers={'Content-Type': 'application/json'})
            with urlopen(req, timeout=30) as response:
                body = response.read()
                return json.loads(body) if 'json' in response.headers.get('Content-Type', '') else body

        def start(log):
            process = subprocess.Popen([str(BINARY)], cwd=root, env=env, stdout=log, stderr=log)
            for _ in range(100):
                try:
                    request('/api/styles')
                    return process
                except OSError:
                    if process.poll() is not None:
                        raise RuntimeError((root / 'server.log').read_text())
                    time.sleep(.1)
            process.terminate()
            process.wait()
            raise RuntimeError('server did not start')

        with (root / 'server.log').open('w+') as log:
            process = start(log)
            try:
                source = root / 'polygon.geojson'
                source.write_text(json.dumps({'type': 'FeatureCollection', 'features': [{
                    'type': 'Feature', 'properties': {}, 'geometry': {'type': 'Polygon',
                    'coordinates': [[[-120, 20], [-80, 20], [-80, 40], [-120, 40], [-120, 20]]]}}]}))
                item = request('/api/catalog', dict(app_id='ai', catalog_path='/test', title='Test',
                    description=None, source_kind='vector', source_config={'path': str(source)},
                    native_crs='EPSG:4326', native_extent=dict(min_x=-120, min_y=20, max_x=-80, max_y=40), attribution=None))
                def style(name, color, app='ai'):
                    return dict(app_id=app, name=name, title=name, description=None,
                                geometry_type='polygon', source_format='geocss',
                                source_text=f'* {{ fill: {color}; stroke: {color}; }}')
                red = request('/api/styles', style('red', '#ff0000'))
                blue = request('/api/styles', style('blue', '#0000ff'))
                request('/api/styles', style('foreign', '#000000', 'other'))
                layer = request('/api/published-layers', dict(id='', app_id='ai', name='polygon', title='Test',
                    description=None, catalog_item_id=item['id'], default_style_id=red['id'],
                    supported_crs=['EPSG:3857'], queryable=False, enabled=True))
                member = dict(published_layer_id=layer['id'], style_id=red['id'], position=0, opacity=1.0)
                group = request('/api/layer-groups', dict(id='', app_id='ai', name='china', title='China',
                    description=None, enabled=True, members=[member],
                    named_styles={'china_dark': [dict(member, style_id=blue['id'])]}))

                def wms(name, selected, tiled=False):
                    query = dict(service='WMS', request='GetMap', version='1.1.1', layers=name,
                                 styles=selected, srs='EPSG:3857', bbox=WORLD,
                                 width=256 if tiled else 280, height=256 if tiled else 280,
                                 format='image/png', transparent='true')
                    if tiled:
                        query['tiled'] = 'true'
                    result = request('/ai/wms?' + urlencode(query))
                    assert result.startswith(b'\x89PNG'), result[:200]
                    return result

                for tiled in (False, True):
                    default = wms('polygon', '', tiled)
                    assert default == wms('polygon', 'default', tiled)
                    assert default == wms('polygon', 'missing', tiled)
                    assert default == wms('polygon', 'foreign', tiled)
                    assert default != wms('polygon', 'blue', tiled)
                    assert wms('polygon', 'blue', tiled) == wms('polygon', 'ai:blue', tiled)
                    default_group = wms('china', '', tiled)
                    assert default_group == wms('china', 'missing', tiled)
                    assert default_group != wms('china', 'china_dark', tiled)
                    assert wms('china', 'china_dark', tiled) == wms('china', 'ai:china_dark', tiled)
                wmts = dict(service='WMTS', request='GetTile', layer='china', style='china_dark',
                            tilematrixset='WebMercatorQuad', tilematrix=0, tilerow=0, tilecol=0)
                assert request('/wmts?' + urlencode(wmts)) == wms('china', 'china_dark', True)
                for endpoint in ('/wms', '/wmts'):
                    xml = request(endpoint + '?service=' + endpoint[1:].upper() + '&request=GetCapabilities')
                    ET.fromstring(xml)
                    assert b'china_dark' in xml
                before = wms('polygon', 'blue', True)
                before_group = wms('china', 'china_dark', True)
                request('/api/styles/' + blue['id'], style('blue', '#00ff00'), 'PUT')
                assert before != wms('polygon', 'blue', True), 'named tile cache was not invalidated'
                assert before_group != wms('china', 'china_dark', True), 'group tile cache was not invalidated'
                cleared = request('/api/published-layers/' + layer['id'] + '/cache', method='DELETE')
                assert cleared['cleared_targets'] >= 2, 'default/named style caches were not cleared'
                try:
                    request('/api/styles/' + blue['id'], method='DELETE')
                    raise AssertionError('referenced group style was deleted')
                except HTTPError as error:
                    assert error.code == 409
                invalid_group = dict(group, named_styles={'broken': [dict(member, style_id='missing')]})
                try:
                    request('/api/layer-groups/' + group['id'], invalid_group, 'PUT')
                    raise AssertionError('missing member style was accepted')
                except HTTPError as error:
                    assert error.code == 422
                process.terminate()
                process.wait(timeout=10)
                process = start(log)
                saved = request('/api/layer-groups')
                assert next(g for g in saved if g['id'] == group['id'])['named_styles'] == group['named_styles']
                assert wms('china', '', True) != wms('china', 'china_dark', True)
                print('PASS: named WMS/tiled WMS/WMTS, fallback, workspace isolation, capabilities, style cache revisions, persistence')
            finally:
                process.terminate()
                process.wait(timeout=10)


if __name__ == '__main__':
    main()
