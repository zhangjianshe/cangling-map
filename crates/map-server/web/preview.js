const $=q=>document.querySelector(q),id=decodeURIComponent(location.pathname.split("/").filter(Boolean)[1]);
let map,imageLayer,wmtsLayer,layerExtent,mode="wms";
async function api(url){const r=await fetch(url);if(!r.ok){let b={};try{b=await r.json()}catch{}throw Error(b.error||`请求失败（${r.status}）`)}return r.json()}
function text(q,v){$(q).textContent=v}
function projection(code,extent){let p=ol.proj.get(code);if(!p){p=new ol.proj.Projection({code,units:/4326|CRS:84/i.test(code)?"degrees":"m",extent});ol.proj.addProjection(p)}return p}
function fit(){if(map&&layerExtent)map.getView().fit(layerExtent,{padding:[48,48,48,48],duration:250,maxZoom:18})}
function setMode(next){mode=next;document.querySelectorAll("[data-mode]").forEach(button=>button.classList.toggle("active",button.dataset.mode===mode));const wms=mode==="wms";imageLayer?.setVisible(wms);wmtsLayer?.setVisible(!wms);$("#mode-empty").classList.add("hidden");$("#fit-extent").disabled=false;text("#preview-mode-label",wms?"WMS · GetMap":"WMTS · WebMercatorQuad");text("#preview-caption",wms?"OpenLayers · WMS GetMap · 默认发布样式":"OpenLayers · WMTS · 默认发布样式");const link=$("#capabilities-link");link.href=wms?"/wms?SERVICE=WMS&REQUEST=GetCapabilities&VERSION=1.3.0":"/wmts?SERVICE=WMTS&REQUEST=GetCapabilities&VERSION=1.0.0";link.textContent=wms?"查看 WMS 能力文档":"查看 WMTS 能力文档";map.updateSize();fit();history.replaceState(null,"",`${location.pathname}?mode=${mode}`)}
async function load(){try{if(typeof ol==="undefined")throw Error("OpenLayers 加载失败");const [layers,catalog]=await Promise.all([api("/api/published-layers"),api("/api/catalog")]),layer=layers.find(x=>x.id===id);if(!layer)throw Error("发布图层不存在");const item=catalog.find(x=>x.id===layer.catalog_item_id);if(!item)throw Error("关联目录项不存在");document.title=`${layer.title} · 图层预览`;text("#title",layer.title);text("#subtitle",layer.description||"使用 OpenLayers 查看发布图层");text("#layer-name",`${layer.app_id}:${layer.name}`);text("#layer-state",layer.enabled?"已启用":"已停用");text("#catalog-title",item.title);text("#native-crs",item.native_crs);text("#output-crs",layer.supported_crs.join(", ")||item.native_crs);const b=item.native_extent,nativeExtent=[b.min_x,b.min_y,b.max_x,b.max_y],nativeProjection=projection(item.native_crs,nativeExtent),webMercator=ol.proj.get("EPSG:3857");layerExtent=ol.proj.transformExtent(nativeExtent,nativeProjection,webMercator);text("#extent",`${b.min_x}, ${b.min_y} — ${b.max_x}, ${b.max_y}`);const imageSource=new ol.source.ImageWMS({url:"/wms",params:{LAYERS:layer.name,STYLES:"",FORMAT:"image/png",TRANSPARENT:true},ratio:1,serverType:"mapserver"});imageLayer=new ol.layer.Image({source:imageSource});imageSource.on("imageloaderror",()=>showError("WMS GetMap 图像生成失败"));const world=webMercator.getExtent(),baseResolution=ol.extent.getWidth(world)/256,resolutions=Array.from({length:23},(_,z)=>baseResolution/Math.pow(2,z)),matrixIds=resolutions.map((_,z)=>String(z)),wmtsSource=new ol.source.WMTS({url:"/wmts",layer:layer.name,matrixSet:"WebMercatorQuad",format:"image/png",style:"default",requestEncoding:"KVP",projection:webMercator,tileGrid:new ol.tilegrid.WMTS({origin:ol.extent.getTopLeft(world),resolutions,matrixIds,tileSize:256}),wrapX:false});wmtsLayer=new ol.layer.Tile({source:wmtsSource,visible:false});map=new ol.Map({target:"preview-map",layers:[imageLayer,wmtsLayer],loadTilesWhileInteracting:true,loadTilesWhileAnimating:true,view:new ol.View({projection:webMercator,center:ol.extent.getCenter(layerExtent),zoom:2})});fit();$("#fit-extent").onclick=fit;document.querySelectorAll("[data-mode]").forEach(button=>button.onclick=()=>setMode(button.dataset.mode));$("#preview-loading").classList.add("hidden");$("#preview-content").classList.remove("hidden");setTimeout(()=>{map.updateSize();const initial=new URLSearchParams(location.search).get("mode");setMode(initial==="wmts"?"wmts":"wms")},0)}catch(e){showError(e.message)}}
function showError(message){$("#preview-loading").classList.add("hidden");$("#preview-content").classList.add("hidden");$("#preview-error").classList.remove("hidden");text("#preview-error-text",message)}
async function setupEditStyleLink(){
 const capabilities=$("#capabilities-link");
 if(!capabilities||$("#edit-style-link"))return;
 const actions=document.createElement("div"),link=document.createElement("a");
 actions.className="preview-meta-actions";
 actions.style.cssText="display:grid;gap:8px";
 link.id="edit-style-link";
 link.className="button-link edit-style-link";
 link.href="/styles";
 link.textContent="编辑样式";
 link.title="进入样式管理";
 link.style.cssText="width:100%;border:1px solid #b8c9c1;background:#fff;color:var(--green);font-weight:700";
 capabilities.before(actions);
 actions.append(link,capabilities);
 try{
  const layers=await api("/api/published-layers"),layer=layers.find(item=>item.id===id);
  if(layer?.default_style_id){
   link.href=`/styles?edit=${encodeURIComponent(layer.default_style_id)}`;
   link.title="编辑当前图层的默认样式";
  }else{
   link.title="当前图层尚未关联默认样式，进入样式管理进行创建或关联";
  }
 }catch{}
}
load();
setupEditStyleLink();
