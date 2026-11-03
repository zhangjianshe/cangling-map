(() => {
class CanglingImageryDownloader extends EventTarget {
  constructor(options) {
    super();
    if (!options?.jobId) throw Error("jobId is required");
    this.jobId = options.jobId;
    this.apiBase = String(options.apiBase ?? window.__canglingBasePath ?? "").replace(/\/$/, "");
    this.workerUrl = options.workerUrl
      || (window.__canglingUrl ? window.__canglingUrl("/assets/imagery-download-worker.js") : `${this.apiBase}/assets/imagery-download-worker.js`);
    this.concurrency = Math.max(1, Math.min(16, Number(options.concurrency) || 4));
    this.pageSize = Math.max(5, Math.min(100, Number(options.pageSize) || 20));
    this.leaseTtlSeconds = Math.max(60, Math.min(3600, Number(options.leaseTtlSeconds) || 3600));
    this.state = "idle";
    this.page = 1;
    this.totalPages = 1;
    this.worker = null;
    this.workerReject = null;
    this.events = null;
    this.heartbeatTimer = null;
    this.looping = false;
  }

  async start() {
    if (!["idle", "paused", "error"].includes(this.state)) return;
    if (this.state === "paused") await this.request(`/api/imagery/jobs/${encodeURIComponent(this.jobId)}/resume`, {method: "POST"});
    const job = await this.request(`/api/imagery/jobs/${encodeURIComponent(this.jobId)}`);
    const repository = await this.request(`/api/imagery/repositories/${encodeURIComponent(job.repository_id)}`);
    if (!['browser', 'hybrid'].includes(job.mode)) throw Error("该任务不允许浏览器下载");
    this.job = job;
    this.repository = repository;
    this.state = "running";
    this.startEvents();
    this.startHeartbeat();
    this.emit("state", {state: this.state});
    return this.continuePages();
  }

  async pause() {
    if (this.state !== "running") return;
    this.state = "paused";
    this.stopWorker(Error("下载已暂停"));
    await this.request(`/api/imagery/jobs/${encodeURIComponent(this.jobId)}/pause`, {method: "POST"});
    this.emit("state", {state: this.state});
  }

  async resume() {
    if (this.state !== "paused") return;
    return this.start();
  }

  async cancel() {
    if (["cancelled", "completed"].includes(this.state)) return;
    this.state = "cancelled";
    this.stopRuntime(Error("下载已取消"));
    await this.request(`/api/imagery/jobs/${encodeURIComponent(this.jobId)}/cancel`, {method: "POST"});
    this.emit("state", {state: this.state});
  }

  async continuePages() {
    if (this.looping) return;
    this.looping = true;
    try {
      while (this.state === "running") {
        const chunks = await this.request(
          `/api/imagery/jobs/${encodeURIComponent(this.jobId)}/chunks?page=1&page_size=${this.pageSize}&status=pending`
        );
        this.totalPages = Math.max(1, chunks.total_pages || 1);
        if (!chunks.items.length) break;
        const lease = await this.request(`/api/imagery/jobs/${encodeURIComponent(this.jobId)}/leases`, {
          method: "POST",
          body: JSON.stringify({ttl_seconds: this.leaseTtlSeconds})
        });
        await this.runPage(chunks.items, lease.token);
        if (this.state === "running") this.page += 1;
      }
      if (this.state === "running") {
        this.state = "completed";
        this.stopRuntime();
        this.emit("state", {state: this.state});
      }
    } catch (error) {
      if (!["paused", "cancelled", "completed", "failed"].includes(this.state)) {
        this.state = "error";
        this.stopWorker();
        this.emit("error", {error});
        this.emit("state", {state: this.state});
        throw error;
      }
    } finally {
      this.looping = false;
    }
  }

  runPage(chunks, lease) {
    return Promise.all(chunks.map(chunk => this.request(
      `/api/imagery/jobs/${encodeURIComponent(this.jobId)}/chunks/${encodeURIComponent(chunk.id)}/missing`
    ))).then(groups => new Promise((resolve, reject) => {
      const tiles = groups.flat();
      if (!tiles.length) {
        Promise.all(chunks.map(chunk => this.completeChunk(chunk.id, lease))).then(resolve, reject);
        return;
      }
      const worker = new Worker(this.workerUrl);
      this.worker = worker;
      this.workerReject = reject;
      worker.onmessage = async event => {
        const message = event.data || {};
        if (message.type === "progress") this.emit("tileprogress", {...message, page: this.page, totalPages: this.totalPages});
        if (message.type === "done") {
          this.workerReject = null;
          this.stopWorker();
          try {
            await Promise.all(chunks.map(chunk => this.completeChunk(chunk.id, lease)));
            resolve(message);
          } catch (error) {
            reject(error);
          }
        }
        if (message.type === "error") {
          this.workerReject = null;
          this.stopWorker();
          reject(Error(message.error || "浏览器瓦片下载失败"));
        }
      };
      worker.onerror = event => {
        this.workerReject = null;
        this.stopWorker();
        reject(Error(event.message || "Web Worker 运行失败"));
      };
      worker.postMessage({
        type: "start",
        chunks,
        tiles,
        concurrency: this.concurrency,
        sourceUrlTemplates: this.repository.source_url_templates
          || String(this.repository.source_url_template || "").split(/\r?\n/).map(value => value.trim()).filter(Boolean),
        format: this.repository.format,
        apiBase: this.apiBase,
        lease
      });
    }));
  }

  async completeChunk(chunkId, lease) {
    let lastError;
    for (let attempt = 1; attempt <= 8; attempt += 1) {
      try {
        return await this.request(
          `/api/imagery/jobs/${encodeURIComponent(this.jobId)}/chunks/${encodeURIComponent(chunkId)}/complete`,
          {method: "POST", headers: {"X-Imagery-Lease": lease}}
        );
      } catch (error) {
        lastError = error;
        if (![409, 503].includes(error.status) || attempt === 8) break;
        await new Promise(resolve => setTimeout(resolve, Math.min(3000, 250 * 2 ** (attempt - 1))));
      }
    }
    throw lastError;
  }

  startEvents() {
    if (this.events) return;
    this.events = new EventSource(`${this.apiBase}/api/imagery/jobs/${encodeURIComponent(this.jobId)}/events`);
    const receive = event => {
      const progress = JSON.parse(event.data);
      this.emit("jobprogress", progress);
      if (event.type === "terminal") {
        this.terminalStatus = progress.job.status;
        if (progress.job.status !== "completed" || this.state !== "running") {
          this.state = progress.job.status;
          this.stopRuntime(Error("任务已结束"));
          this.emit("state", {state: this.state});
        }
      }
    };
    this.events.addEventListener("progress", receive);
    this.events.addEventListener("terminal", receive);
    this.events.onerror = () => this.emit("warning", {message: "任务进度连接已中断，浏览器将自动重连"});
  }

  startHeartbeat() {
    if (this.heartbeatTimer) return;
    const heartbeat = () => this.request(`/api/imagery/jobs/${encodeURIComponent(this.jobId)}/heartbeat`, {method: "POST"})
      .catch(error => this.emit("warning", {message: `任务心跳失败：${error.message}`}));
    heartbeat();
    this.heartbeatTimer = setInterval(heartbeat, 30000);
  }

  stopWorker(error = null) {
    if (!this.worker) return;
    const reject = this.workerReject;
    this.workerReject = null;
    this.worker.postMessage({type: "cancel"});
    this.worker.terminate();
    this.worker = null;
    if (error && reject) reject(error);
  }

  stopRuntime(error = null) {
    this.stopWorker(error);
    if (this.events) this.events.close();
    this.events = null;
    if (this.heartbeatTimer) clearInterval(this.heartbeatTimer);
    this.heartbeatTimer = null;
  }

  async request(path, options = {}) {
    const headers = {...options.headers};
    if (options.body) headers["Content-Type"] = "application/json";
    const url = this.apiBase && path.startsWith("/") && !path.startsWith(`${this.apiBase}/`)
      ? `${this.apiBase}${path}` : path;
    const response = await fetch(url, {...options, headers});
    if (!response.ok) {
      let body = {};
      try { body = await response.json(); } catch {}
      const error = Error(body.error || `请求失败（${response.status}）`);
      error.status = response.status;
      throw error;
    }
    return response.status === 204 ? null : response.json();
  }

  emit(type, detail) {
    this.dispatchEvent(new CustomEvent(type, {detail}));
  }
}

window.CanglingImageryDownloader = CanglingImageryDownloader;
})();
