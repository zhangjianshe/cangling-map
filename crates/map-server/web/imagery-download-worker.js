let cancelled = false;
const activeRequests = new Set();

self.onmessage = event => {
  const message = event.data || {};
  if (message.type === "cancel") {
    cancelled = true;
    activeRequests.forEach(controller => controller.abort());
    activeRequests.clear();
    return;
  }
  if (message.type === "start") {
    cancelled = false;
    run(message).catch(error => {
      if (!cancelled) self.postMessage({type: "error", error: error.message || String(error)});
    });
  }
};

async function run(config) {
  const chunks = Array.isArray(config.chunks) ? config.chunks : [];
  const explicitTiles = Array.isArray(config.tiles) ? config.tiles : null;
  const concurrency = Math.max(1, Math.min(16, Number(config.concurrency) || 4));
  const cursor = {tile: 0, chunk: 0, x: null, y: null};
  const total = explicitTiles ? explicitTiles.length : chunks.reduce((sum, chunk) => sum
    + (chunk.max_col - chunk.min_col + 1) * (chunk.max_row - chunk.min_row + 1), 0);
  let completed = 0;
  let lastReport = 0;

  const nextTile = () => {
    if (explicitTiles) return explicitTiles[cursor.tile++] || null;
    while (cursor.chunk < chunks.length) {
      const chunk = chunks[cursor.chunk];
      if (cursor.x === null) {
        cursor.x = chunk.min_col;
        cursor.y = chunk.min_row;
      }
      if (cursor.y > chunk.max_row) {
        cursor.chunk += 1;
        cursor.x = null;
        cursor.y = null;
        continue;
      }
      const tile = {z: chunk.zoom, x: cursor.x, y: cursor.y};
      cursor.x += 1;
      if (cursor.x > chunk.max_col) {
        cursor.x = chunk.min_col;
        cursor.y += 1;
      }
      return tile;
    }
    return null;
  };

  const runner = async () => {
    while (!cancelled) {
      const tile = nextTile();
      if (!tile) return;
      await transferTile(config, tile);
      completed += 1;
      const now = Date.now();
      if (completed === total || completed % 10 === 0 || now - lastReport >= 500) {
        lastReport = now;
        self.postMessage({type: "progress", completed, total, tile});
      }
    }
  };
  await Promise.all(Array.from({length: concurrency}, runner));
  if (!cancelled) self.postMessage({type: "done", completed, total});
}

async function transferTile(config, tile) {
  const templates = Array.isArray(config.sourceUrlTemplates) ? config.sourceUrlTemplates.filter(Boolean) : [];
  if (!templates.length) throw Error("影像仓库没有可用的源瓦片 URL");
  const start = ((tile.z * 31 + tile.x) * 31 + tile.y) % templates.length;
  const attempts = Math.max(3, templates.length);
  let sourceUrl = "";
  let lastError;
  for (let attempt = 0; attempt < attempts && !cancelled; attempt += 1) {
    sourceUrl = templates[(start + attempt) % templates.length]
      .replaceAll("{z}", String(tile.z))
      .replaceAll("{x}", String(tile.x))
      .replaceAll("{y}", String(tile.y));
    const controller = new AbortController();
    activeRequests.add(controller);
    try {
      const source = await fetch(sourceUrl, {cache: "no-store", signal: controller.signal});
      if (!source.ok) throw Error(`源瓦片返回 ${source.status}: ${sourceUrl}`);
      const bytes = await source.arrayBuffer();
      if (!bytes.byteLength || bytes.byteLength > 10 * 1024 * 1024) {
        throw Error(`源瓦片大小无效: ${sourceUrl}`);
      }
      const extension = config.format === "jpeg" ? "jpg" : config.format;
      const uploadUrl = `${config.apiBase}/api/imagery/uploads/${tile.z}/${tile.x}/${tile.y}.${extension}`;
      const upload = await fetch(uploadUrl, {
        method: "PUT",
        headers: {
          "Content-Type": source.headers.get("Content-Type") || contentType(config.format),
          "X-Imagery-Lease": config.lease
        },
        body: bytes,
        signal: controller.signal
      });
      if (!upload.ok) {
        let detail = "";
        try { detail = (await upload.json()).error || ""; } catch {}
        throw Error(`上传瓦片失败（${upload.status}）${detail ? `: ${detail}` : ""}`);
      }
      return;
    } catch (error) {
      if (cancelled || error.name === "AbortError") throw error;
      lastError = error;
      if (attempt + 1 < attempts) await delay(Math.min(2000, 250 * 2 ** attempt));
    } finally {
      activeRequests.delete(controller);
    }
  }
  throw lastError || Error(`瓦片处理失败: ${sourceUrl}`);
}

function contentType(format) {
  if (format === "jpeg") return "image/jpeg";
  if (format === "webp") return "image/webp";
  return "image/png";
}

function delay(milliseconds) {
  return new Promise(resolve => setTimeout(resolve, milliseconds));
}
