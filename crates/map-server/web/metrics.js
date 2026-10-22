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
  renderHardware(data);
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

function renderHardware(data) {
  const hardware = data.hardware || {};
  const cpu = hardware.cpu || {};
  const memory = hardware.memory || {};
  const gpu = data.gpu_renderer;
  const mode = String(data.render_mode || 'cpu').toLowerCase();
  const backend = String(data.vector_render_backend || mode);
  const gpuActive = Boolean(gpu) && !gpu.primary_disabled;
  const cpuActive = mode === 'cpu' || backend.includes('cpu fallback') || Boolean(gpu?.primary_disabled);
  byId('render-mode').textContent = mode === 'auto' ? 'AUTO · GPU → CPU' : mode.toUpperCase();
  byId('render-backend').textContent = `实际后端：${backend}`;
  byId('render-flow').innerHTML = mode === 'auto'
    ? `<b class="${gpuActive ? 'active' : ''}">GPU</b><i>→</i><b class="${cpuActive ? 'active' : ''}">CPU</b>`
    : `<b class="active">${mode === 'gpu' ? 'GPU' : 'CPU'}</b>`;

  const health = byId('renderer-health');
  if (gpu?.primary_disabled || backend.includes('cpu fallback')) {
    health.textContent = 'GPU 已降级';
    health.className = 'renderer-health warn';
  } else if (mode === 'gpu' && !gpu) {
    health.textContent = 'GPU 不可用';
    health.className = 'renderer-health danger';
  } else {
    health.textContent = gpuActive ? 'GPU 正常' : 'CPU 正常';
    health.className = 'renderer-health good';
  }

  const cpuUsage = cpu.utilization_percent;
  byId('cpu-usage').textContent = cpuUsage == null ? '采样中…' : `${Number(cpuUsage).toFixed(1)}%`;
  byId('cpu-model').textContent = cpu.model || '未知处理器';
  const load = [cpu.load_average_1m, cpu.load_average_5m, cpu.load_average_15m]
    .map(value => value == null ? '—' : Number(value).toFixed(2)).join(' / ');
  byId('cpu-detail').textContent = `${integer.format(cpu.physical_cores || 0)} 核 · ${integer.format(cpu.logical_cores || 0)} 线程 · 负载 ${load}`;

  const totalMemory = Number(memory.total_bytes || 0);
  const usedMemory = Number(memory.used_bytes || 0);
  const memoryPercent = totalMemory ? usedMemory / totalMemory * 100 : 0;
  byId('memory-usage').textContent = `${memoryPercent.toFixed(1)}%`;
  byId('memory-detail').textContent = `${bytes(usedMemory)} / ${bytes(totalMemory)} · 可用 ${bytes(Number(memory.available_bytes || 0))}`;
  byId('memory-progress').value = memoryPercent;

  byId('gpu-request-stats').textContent = gpu
    ? `GPU 成功 ${integer.format(gpu.primary_successes)} · 降级 ${integer.format(gpu.fallback_requests)} · 失败 ${integer.format(gpu.primary_failures)}`
    : '当前未启用 GPU 渲染';
  const probeNote = byId('gpu-probe-note');
  probeNote.textContent = hardware.gpu_telemetry_error
    ? `适配器枚举正常；实时遥测不可用：${hardware.gpu_telemetry_error}`
    : '枚举图形适配器，并读取 NVIDIA 显存、利用率、温度和功耗。';
  renderGpuAdapters(hardware.gpu_adapters || []);
}

function renderGpuAdapters(adapters) {
  const body = byId('gpu-adapters');
  if (!adapters.length) {
    body.innerHTML = '<tr><td colspan="7" class="metrics-empty">未发现可用 GPU 适配器</td></tr>';
    return;
  }
  body.innerHTML = adapters.map(adapter => {
    const used = adapter.memory_used_bytes;
    const total = adapter.memory_total_bytes;
    const memory = used == null || total == null ? '—' : `${bytes(used)} / ${bytes(total)}`;
    const utilization = adapter.utilization_percent == null ? null : Number(adapter.utilization_percent);
    const temperature = adapter.temperature_celsius == null ? '—' : `${Number(adapter.temperature_celsius).toFixed(0)} °C`;
    const power = adapter.power_watts == null ? '—' : `${Number(adapter.power_watts).toFixed(1)} W`;
    return `<tr><td><div class="gpu-device-name"><i class="${adapter.selected ? 'selected' : ''}"></i><span><b>${escapeHtml(adapter.name)}</b><small>${adapter.selected ? '当前渲染设备' : '可用适配器'}</small></span></div></td><td>${escapeHtml(adapter.backend)}<small>${escapeHtml(adapter.device_type)}</small></td><td><span class="gpu-status-pill ${adapter.selected ? 'active' : ''}">${adapter.selected ? '使用中' : '待命'}</span></td><td>${memory}</td><td>${utilization == null ? '—' : `<div class="gpu-usage"><span>${utilization.toFixed(1)}%</span><progress max="100" value="${utilization}"></progress></div>`}</td><td>${temperature}<small>${power}</small></td><td>${escapeHtml(adapter.driver_version || '—')}</td></tr>`;
  }).join('');
}

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
