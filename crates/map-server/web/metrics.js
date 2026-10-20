document.head.append(Object.assign(document.createElement("script"),{src:"/assets/service-frame.js"}));
const byId = id => document.getElementById(id);
const integer = new Intl.NumberFormat('zh-CN');

function duration(seconds) {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor(seconds % 86400 / 3600);
  const minutes = Math.floor(seconds % 3600 / 60);
  if (days) return `${days} 天 ${hours} 小时`;
  if (hours) return `${hours} 小时 ${minutes} 分钟`;
  return `${minutes} 分钟`;
}

function render(data) {
  const lookups = data.cache_hits + data.cache_misses;
  const rate = lookups ? data.cache_hits / lookups * 100 : 0;
  byId('wmts-requests').textContent = integer.format(data.wmts_requests);
  byId('uptime').textContent = `服务运行时间 ${duration(data.uptime_seconds)}`;
  byId('cache-rate').textContent = `${rate.toFixed(1)}%`;
  byId('cache-detail').textContent = `命中 ${integer.format(data.cache_hits)} · 未命中 ${integer.format(data.cache_misses)}`;
  byId('rendered-tiles').textContent = integer.format(data.rendered_tiles);
  byId('render-failures').textContent = `失败 ${integer.format(data.render_failures)}`;
  byId('coalesced').textContent = integer.format(data.coalesced_requests);
  byId('cache-backend').textContent = data.cache_backend.label;
  byId('cache-backend-detail').textContent = data.cache_backend.detail;
  byId('cache-backend').dataset.backend = data.cache_backend.backend;
  byId('render-ms').textContent = `${data.average_render_ms.toFixed(2)} ms`;
  byId('cache-write-ms').textContent = `${data.average_cache_write_ms.toFixed(2)} ms`;
  byId('active-requests').textContent = integer.format(data.active_tile_requests);
  byId('available-concurrency').textContent = integer.format(data.render_concurrency_available);
  byId('projection-budget').textContent = `${integer.format(data.projection_cache_budget_mb)} MB`;
  renderLayers(data.layers);
  renderSlowRequests(data.slow_requests);
  renderSeries(data.series);
  renderCacheStatus(data.render_cache);
  byId('updated').textContent = `更新于 ${new Date().toLocaleTimeString('zh-CN', {hour12:false})}`;
}

const escapeHtml = value => String(value).replace(/[&<>"']/g, char => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[char]));
const milliseconds = value => `${Number(value).toFixed(2)} ms`;
const percent = value => `${(Number(value) * 100).toFixed(1)}%`;
const bytes = value => value >= 1048576 ? `${(value / 1048576).toFixed(1)} MB` : value >= 1024 ? `${(value / 1024).toFixed(1)} KB` : `${value} B`;

function renderLayers(layers) {
  const body = byId('layer-metrics');
  if (!layers.length) {
    body.innerHTML = '<tr><td colspan="8" class="metrics-empty">尚无图层请求</td></tr>';
    return;
  }
  body.innerHTML = layers.map(layer => `<tr><td><a class="style-link" href="/published-layers/${encodeURIComponent(layer.id)}/preview">${escapeHtml(layer.title || layer.name)}</a><small class="metric-layer-name">${escapeHtml(layer.name)}</small></td><td>${integer.format(layer.requests)}</td><td>${percent(layer.cache_hit_rate)}</td><td>${integer.format(layer.rendered_tiles)}<small>${integer.format(layer.coalesced_requests)} 个合并</small></td><td>${milliseconds(layer.average_render_ms)}<small>P95 ${milliseconds(layer.p95_render_ms)}</small></td><td>${milliseconds(layer.average_preparation_ms)}<small>${milliseconds(layer.average_cache_write_ms)}</small></td><td>${bytes(layer.output_bytes)}</td><td class="${layer.failures ? 'metric-danger' : ''}">${integer.format(layer.failures)}</td></tr>`).join('');
}

function renderSlowRequests(requests) {
  const body = byId('slow-requests');
  if (!requests.length) {
    body.innerHTML = '<tr><td colspan="8" class="metrics-empty">尚无慢请求</td></tr>';
    return;
  }
  body.innerHTML = requests.map(item => `<tr><td>${new Date(item.timestamp * 1000).toLocaleTimeString('zh-CN',{hour12:false})}</td><td>${escapeHtml(item.layer)}</td><td class="mono">${item.zoom}/${item.col}/${item.row}</td><td><b>${milliseconds(item.total_ms)}</b></td><td>${milliseconds(item.preparation_ms)}</td><td>${milliseconds(item.render_ms)}</td><td>${milliseconds(item.cache_write_ms)}</td><td><a class="style-link" href="/published-layers/${encodeURIComponent(item.layer_id)}/preview">查看</a></td></tr>`).join('');
}

function renderSeries(series) {
  const maxRequests = Math.max(1, ...series.map(point => point.requests));
  byId('metric-series').innerHTML = series.map(point => {
    const height = Math.max(3, point.requests / maxRequests * 100);
    const time = new Date(point.timestamp * 1000).toLocaleTimeString('zh-CN',{hour:'2-digit',minute:'2-digit',hour12:false});
    return `<div class="series-column" title="${time} · ${point.requests} 请求 · 命中率 ${percent(point.cache_hit_rate)} · ${milliseconds(point.average_duration_ms)}"><div class="series-bar" style="height:${height}%"></div><small>${time}</small></div>`;
  }).join('');
}

function renderCacheStatus(cache) {
  const projectionRate = cache.projection_hits + cache.projection_misses ? cache.projection_hits / (cache.projection_hits + cache.projection_misses) : 0;
  const patternRate = cache.pattern_hits + cache.pattern_misses ? cache.pattern_hits / (cache.pattern_hits + cache.pattern_misses) : 0;
  byId('cache-status').innerHTML = `<article><span>投影数据集</span><strong>${cache.projection_entries} 项 · ${bytes(cache.projection_estimated_bytes)}</strong><small>命中率 ${percent(projectionRate)} · 淘汰 ${integer.format(cache.projection_evictions)}</small><progress max="${cache.projection_budget_bytes}" value="${cache.projection_estimated_bytes}"></progress></article><article><span>图片纹理</span><strong>${cache.pattern_entries} 项 · ${bytes(cache.pattern_estimated_bytes)}</strong><small>命中率 ${percent(patternRate)} · 命中 ${integer.format(cache.pattern_hits)}</small></article><article><span>源数据集池</span><strong>${cache.source_entries} 项</strong><small>命中 ${integer.format(cache.source_hits)} · 未命中 ${integer.format(cache.source_misses)}</small></article>`;
}

async function load() {
  const error = byId('metrics-error');
  try {
    const response = await fetch('/api/metrics', {cache: 'no-store'});
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    render(await response.json());
    error.classList.add('hidden');
  } catch (reason) {
    error.textContent = `无法获取服务指标：${reason.message}`;
    error.classList.remove('hidden');
  }
}

byId('refresh').addEventListener('click', load);
load();
setInterval(load, 5000);
