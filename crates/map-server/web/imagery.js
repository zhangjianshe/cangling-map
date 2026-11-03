(() => {
  const $ = selector => document.querySelector(selector);
  const state = {
    repositories: [], repositoryPage: 1, repositoryPageSize: 20, repositoryPages: 1,
    jobs: [], jobPage: 1, jobPageSize: 20, jobPages: 1,
    repositoryQuery: "", editingRepository: null, selectedRepository: null,
    deletingRepository: null, deletingJob: null, currentJob: null, downloader: null, events: null
  };
  const statusNames = {pending: "等待中", running: "进行中", paused: "已暂停", completed: "已完成", failed: "失败", cancelled: "已取消"};
  const modeNames = {browser: "浏览器", hybrid: "混合", server: "服务端"};
  const terminal = new Set(["completed", "failed", "cancelled"]);

  async function api(path, options = {}) {
    const headers = {...options.headers};
    if (options.body) headers["Content-Type"] = "application/json";
    const response = await fetch(path, {...options, headers});
    if (!response.ok) {
      let body = {};
      try { body = await response.json(); } catch {}
      throw Error(body.error || `请求失败（${response.status}）`);
    }
    return response.status === 204 ? null : response.json();
  }
  const esc = value => String(value ?? "").replace(/[&<>"']/g, char => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[char]));
  const fmtDate = value => value ? new Date(value).toLocaleString("zh-CN", {hour12:false}) : "—";
  const fmtNumber = value => Number(value || 0).toLocaleString("zh-CN");
  function absoluteBrowserUrl(value) {
    try {
      const path = String(value || "");
      return new URL(path.startsWith("/") || /^[a-z][a-z\d+.-]*:/i.test(path) || path.startsWith("//") ? path : `/${path}`, window.location.origin).href.replace(/%7B([zxy])%7D/gi, "{$1}");
    } catch { return String(value || ""); }
  }
  function fallbackCopyText(value) {
    const field = document.createElement("textarea");
    field.value = String(value ?? ""); field.readOnly = true;
    field.style.cssText = "position:fixed;left:-9999px;top:0;opacity:0";
    document.body.append(field); field.select(); field.setSelectionRange(0, field.value.length);
    let copied = false;
    try { copied = document.execCommand("copy"); } catch {}
    field.remove(); return copied;
  }
  async function copyText(value) {
    if (window.isSecureContext && navigator.clipboard?.writeText) {
      try { await navigator.clipboard.writeText(String(value ?? "")); return true; } catch {}
    }
    return fallbackCopyText(value);
  }
  function toast(message, bad = false) {
    const element = $("#toast"); element.textContent = message; element.className = bad ? "show bad" : "show";
    clearTimeout(toast.timer); toast.timer = setTimeout(() => element.className = "", 2600);
  }
  function showError(selector, message = "") {
    const element = $(selector); element.textContent = message; element.classList.toggle("hidden", !message);
  }
  function repositoryById(id) { return state.repositories.find(item => item.id === id); }

  async function loadLeadership() {
    try {
      const snapshot = await api("/api/imagery/leadership");
      const leader = snapshot.leader_id || (snapshot.is_leader ? snapshot.candidate_id : "尚未选出");
      $("#leader-name").textContent = snapshot.mode === "standalone" ? "单实例" : leader;
      $("#leader-detail").textContent = snapshot.mode === "standalone" ? "无需集群选举" : `${snapshot.is_leader ? "当前实例是主节点" : "跟随主节点"} · 任期 ${snapshot.term}`;
      $("#worker-concurrency").textContent = snapshot.worker.enabled ? `${snapshot.worker.effective_concurrency} / ${snapshot.worker.concurrency}` : "未启用";
      $("#worker-detail").textContent = `最低并发 ${snapshot.worker.min_concurrency}`;
      $("#worker-latency").textContent = `${fmtNumber(snapshot.worker.average_tile_ms)} ms`;
      $("#worker-tiles").textContent = `已处理 ${fmtNumber(snapshot.worker.processed_tiles)} 张`;
      $("#active-chunks").textContent = fmtNumber(snapshot.worker.active_chunks);
      $("#chunk-detail").textContent = `完成 ${fmtNumber(snapshot.worker.completed_chunks)} · 失败 ${fmtNumber(snapshot.worker.failed_attempts)}`;
    } catch (error) {
      $("#leader-name").textContent = "状态不可用"; $("#leader-detail").textContent = error.message;
    }
  }

  function pageMarkup(page, pages, total, pageSize) {
    return `<div class="pagination-info"><span>共 ${fmtNumber(total)} 条</span><label>每页 <select data-page-size><option ${pageSize===10?"selected":""}>10</option><option ${pageSize===20?"selected":""}>20</option><option ${pageSize===50?"selected":""}>50</option><option ${pageSize===100?"selected":""}>100</option></select> 条</label></div><div class="pagination-controls"><button type="button" data-prev ${page<=1?"disabled":""}>上一页</button><span>${page} / ${Math.max(1,pages)}</span><button type="button" data-next ${page>=pages?"disabled":""}>下一页</button></div>`;
  }
  function bindPager(selector, kind, total) {
    const element = $(selector), pageKey = `${kind}Page`, sizeKey = `${kind}PageSize`, pagesKey = `${kind}Pages`, loader = kind === "repository" ? loadRepositories : loadJobs;
    element.innerHTML = pageMarkup(state[pageKey], state[pagesKey], total, state[sizeKey]);
    element.querySelector("[data-prev]").onclick = () => { state[pageKey]--; loader(); };
    element.querySelector("[data-next]").onclick = () => { state[pageKey]++; loader(); };
    element.querySelector("[data-page-size]").onchange = event => { state[sizeKey] = Number(event.target.value); state[pageKey] = 1; loader(); };
  }

  async function loadRepositories() {
    const params = new URLSearchParams({page: state.repositoryPage, page_size: state.repositoryPageSize});
    if (state.repositoryQuery) params.set("q", state.repositoryQuery);
    try {
      const result = await api(`/api/imagery/repositories?${params}`);
      state.repositories = result.items; state.repositoryPage = result.page; state.repositoryPages = result.total_pages;
      $("#repository-summary").textContent = `共 ${fmtNumber(result.total)} 个仓库 · 第 ${result.page}/${Math.max(1,result.total_pages)} 页`;
      $("#repository-rows").innerHTML = result.items.length ? result.items.map(repositoryRow).join("") : '<tr><td colspan="7" class="empty-row">还没有影像仓库，请先添加数据源。</td></tr>';
      bindPager("#repository-pagination", "repository", result.total);
    } catch (error) {
      $("#repository-rows").innerHTML = `<tr><td colspan="7" class="empty-row">无法加载：${esc(error.message)}</td></tr>`;
    }
  }
  function repositoryRow(item) {
    const bounds = item.bounds.map(value => Number(value).toFixed(2));
    const sourceTemplates = item.source_url_templates || String(item.source_url_template || "").split(/\r?\n/).filter(Boolean);
    const sourceSummary = item.description || `${sourceTemplates[0] || "未配置下载源"}${sourceTemplates.length > 1 ? ` · 另有 ${sourceTemplates.length - 1} 个备用源` : ""}`;
    const browserUrl = absoluteBrowserUrl(item.browser_url);
    const previewUrl = window.__canglingUrl(`/imagery/repositories/${encodeURIComponent(item.id)}/preview`);
    const updated = fmtDate(item.updated_at).split(" ");
    return `<tr><td><div class="imagery-name"><i>▧</i><span><b>${esc(item.name)}</b><small title="${esc(sourceTemplates.join("\n"))}">${esc(sourceSummary)}</small></span></div></td><td><div class="repository-spec"><b>Z${item.min_zoom}–Z${item.max_zoom}</b><span class="imagery-badge">${esc(String(item.format).toUpperCase())}</span></div></td><td><div class="repository-bounds"><span><small>西南</small>${esc(bounds[0])}, ${esc(bounds[1])}</span><span><small>东北</small>${esc(bounds[2])}, ${esc(bounds[3])}</span></div></td><td class="repository-location"><code title="${esc(browserUrl)}">${esc(browserUrl)}</code><div class="repository-location-actions"><a href="${esc(previewUrl)}">预览 ↗</a><button type="button" data-copy="${esc(browserUrl)}">复制</button></div></td><td class="repository-location"><code title="${esc(item.storage_path)}">${esc(item.storage_path)}</code><button type="button" data-copy="${esc(item.storage_path)}">复制</button></td><td class="repository-version"><b>v${item.version}</b><small>${esc(updated[0] || "—")}<br>${esc(updated.slice(1).join(" "))}</small></td><td><div class="imagery-actions"><button title="新建下载任务" data-action="job" data-id="${item.id}">下载</button><button data-action="edit" data-id="${item.id}">编辑</button><button class="danger" data-action="delete" data-id="${item.id}">删除</button></div></td></tr>`;
  }

  async function loadJobs() {
    try {
      const result = await api(`/api/imagery/jobs?page=${state.jobPage}&page_size=${state.jobPageSize}`);
      state.jobs = result.items; state.jobPage = result.page; state.jobPages = result.total_pages;
      $("#job-summary").textContent = `共 ${fmtNumber(result.total)} 个任务 · 第 ${result.page}/${Math.max(1,result.total_pages)} 页`;
      const missing = [...new Set(result.items.map(item => item.repository_id).filter(id => !repositoryById(id)))];
      if (missing.length) await Promise.all(missing.map(id => api(`/api/imagery/repositories/${encodeURIComponent(id)}`).then(item => state.repositories.push(item)).catch(() => {})));
      $("#job-rows").innerHTML = result.items.length ? result.items.map(jobRow).join("") : '<tr><td colspan="5" class="empty-row">还没有下载任务。</td></tr>';
      bindPager("#job-pagination", "job", result.total);
      if (state.currentJob) {
        const fresh = result.items.find(item => item.id === state.currentJob.id);
        if (fresh) updateProgress(fresh);
      }
    } catch (error) {
      $("#job-rows").innerHTML = `<tr><td colspan="5" class="empty-row">无法加载：${esc(error.message)}</td></tr>`;
    }
  }
  function progressValue(job) { return job.total_tiles ? Math.min(100, Math.round((job.completed_tiles + job.failed_tiles + job.skipped_tiles) * 100 / job.total_tiles)) : 0; }
  function jobRow(job) {
    const repository = repositoryById(job.repository_id), percent = progressValue(job), badge = job.status === "completed" ? "good" : job.status === "failed" || job.status === "cancelled" ? "bad" : job.status === "paused" ? "warn" : "";
    const browserAction = ["browser", "hybrid"].includes(job.mode) && !terminal.has(job.status) ? `<button data-job-action="download" data-id="${job.id}">浏览器下载</button>` : "";
    const pauseAction = job.status === "running" ? `<button data-job-action="pause" data-id="${job.id}">暂停</button>` : job.status === "paused" ? `<button data-job-action="resume" data-id="${job.id}">恢复</button>` : "";
    const takeover = job.mode === "hybrid" && job.status === "running" ? `<button data-job-action="takeover" data-id="${job.id}">接管</button>` : "";
    const cancel = !terminal.has(job.status) ? `<button class="danger" data-job-action="cancel" data-id="${job.id}">取消</button>` : "";
    const remove = `<button class="danger" data-job-action="delete" data-id="${job.id}">删除</button>`;
    const scope = Array.isArray(job.bounds) ? `Z${job.min_zoom}–Z${job.max_zoom} · ${job.bounds.map(value => Number(value).toFixed(2)).join(", ")}` : job.id;
    const updated = fmtDate(job.updated_at).split(" ");
    const failures = job.failed_tiles ? ` · 失败 ${fmtNumber(job.failed_tiles)}` : "";
    const skipped = job.skipped_tiles ? ` · 跳过 ${fmtNumber(job.skipped_tiles)}` : "";
    return `<tr><td><div class="imagery-name"><i>⇩</i><span><b>${esc(repository?.name || job.repository_id)}</b><small title="任务 ${esc(job.id)}">${esc(scope)}</small></span></div></td><td><div class="job-state"><span class="imagery-badge">${modeNames[job.mode] || job.mode}</span><span class="imagery-badge ${badge}">${statusNames[job.status] || job.status}</span>${job.error ? `<small title="${esc(job.error)}" style="color:#bd3e3e">${esc(job.error)}</small>` : ""}</div></td><td class="progress-cell"><div class="job-progress-head"><b>${percent}%</b><small>${fmtNumber(job.completed_tiles)} / ${fmtNumber(job.total_tiles)} 张</small></div><progress max="100" value="${percent}"></progress><small>完成 ${fmtNumber(job.completed_tiles)}${failures}${skipped}</small></td><td class="job-updated"><b>${esc(updated[0] || "—")}</b><small>${esc(updated.slice(1).join(" "))}</small></td><td><div class="imagery-actions"><button data-job-action="details" data-id="${job.id}">详情</button>${browserAction}${pauseAction}${takeover}${cancel}${remove}</div></td></tr>`;
  }

  function openRepository(item = null) {
    state.editingRepository = item; const form = $("#repository-form"); form.reset(); showError("#repository-error");
    $("#repository-title").textContent = item ? "编辑影像仓库" : "添加影像仓库";
    if (item) {
      for (const key of ["name","description","min_zoom","max_zoom","format"]) form.elements[key].value = item[key] ?? "";
      form.elements.source_url_template.value = (item.source_url_templates || String(item.source_url_template || "").split(/\r?\n/).filter(Boolean)).join("\n");
      [form.elements.min_x.value, form.elements.min_y.value, form.elements.max_x.value, form.elements.max_y.value] = item.bounds;
      form.elements.revocable.checked = item.revocable;
    } else { form.elements.min_zoom.value = 0; form.elements.max_zoom.value = 19; form.elements.min_x.value = 73.5; form.elements.min_y.value = 3.84; form.elements.max_x.value = 135.09; form.elements.max_y.value = 53.56; }
    $("#repository-dialog").showModal();
  }
  async function saveRepository() {
    const form = $("#repository-form"); if (!form.reportValidity()) return;
    const button = $("#save-repository"); button.disabled = true; showError("#repository-error");
    const input = {name:form.elements.name.value.trim(), description:form.elements.description.value.trim() || null, source_url_template:form.elements.source_url_template.value.trim(), min_zoom:Number(form.elements.min_zoom.value), max_zoom:Number(form.elements.max_zoom.value), bounds:["min_x","min_y","max_x","max_y"].map(key => Number(form.elements[key].value)), format:form.elements.format.value, revocable:form.elements.revocable.checked};
    try {
      const path = state.editingRepository ? `/api/imagery/repositories/${encodeURIComponent(state.editingRepository.id)}` : "/api/imagery/repositories";
      await api(path, {method: state.editingRepository ? "PUT" : "POST", body:JSON.stringify(input)});
      $("#repository-dialog").close(); toast(state.editingRepository ? "影像仓库已更新" : "影像仓库已创建"); await loadRepositories();
    } catch (error) { showError("#repository-error", error.message); } finally { button.disabled = false; }
  }

  function openJob(repository) {
    state.selectedRepository = repository; const form = $("#job-form"); form.reset();
    $("#job-repository-name").textContent = repository.name;
    form.elements.min_zoom.value = repository.min_zoom; form.elements.max_zoom.value = repository.max_zoom;
    [form.elements.min_x.value, form.elements.min_y.value, form.elements.max_x.value, form.elements.max_y.value] = repository.bounds;
    showError("#job-error"); $("#job-dialog").showModal();
  }
  async function createJob() {
    const form = $("#job-form"); if (!form.reportValidity()) return;
    const button = $("#create-job"), mode = new FormData(form).get("mode"); button.disabled = true; showError("#job-error");
    try {
      const input = {repository_id:state.selectedRepository.id, mode, min_zoom:Number(form.elements.min_zoom.value), max_zoom:Number(form.elements.max_zoom.value), bounds:["min_x","min_y","max_x","max_y"].map(key => Number(form.elements[key].value)), total_tiles:0};
      const job = await api("/api/imagery/jobs", {method:"POST", body:JSON.stringify(input)});
      $("#job-dialog").close(); toast("下载任务已创建"); switchTab("jobs"); await loadJobs(); openProgress(job);
      if (["browser","hybrid"].includes(job.mode)) startBrowserDownload(job);
    } catch (error) { showError("#job-error", error.message); } finally { button.disabled = false; }
  }

  function openProgress(job) {
    state.currentJob = job; updateProgress(job); $("#progress-dialog").showModal(); subscribeProgress(job.id);
  }
  function updateProgress(job, message = "") {
    state.currentJob = job; const repository = repositoryById(job.repository_id), percent = progressValue(job);
    $("#progress-title").textContent = repository?.name || "影像下载任务"; $("#job-progress").value = percent;
    $("#progress-completed").textContent = fmtNumber(job.completed_tiles); $("#progress-failed").textContent = fmtNumber(job.failed_tiles); $("#progress-skipped").textContent = fmtNumber(job.skipped_tiles); $("#progress-total").textContent = fmtNumber(job.total_tiles);
    $("#progress-state").textContent = `${statusNames[job.status] || job.status} · ${percent}%`; $("#progress-state-dot").className = `leader-dot ${job.status === "completed" ? "good" : job.status === "failed" ? "bad" : ""}`;
    $("#progress-message").textContent = message || job.error || (job.mode === "browser" ? "浏览器负责下载和上传瓦片" : job.mode === "hybrid" ? "浏览器优先，必要时可由服务端接管" : "服务端工作器正在处理任务");
    $("#progress-updated").textContent = `更新于 ${fmtDate(job.updated_at)}`; $("#progress-pause").textContent = job.status === "paused" ? "恢复" : "暂停"; $("#progress-pause").disabled = terminal.has(job.status);
    $("#progress-cancel").disabled = terminal.has(job.status); $("#progress-takeover").classList.toggle("hidden", !(job.mode === "hybrid" && job.status === "running"));
  }
  function subscribeProgress(id) {
    state.events?.close(); const base = window.__canglingBasePath || ""; state.events = new EventSource(`${base}/api/imagery/jobs/${encodeURIComponent(id)}/events`);
    const receive = event => { const progress = JSON.parse(event.data); updateProgress(progress.job); if (event.type === "terminal") { state.events?.close(); state.events = null; loadJobs(); } };
    state.events.addEventListener("progress", receive); state.events.addEventListener("terminal", receive);
    state.events.onerror = () => { if (state.currentJob?.id === id && !terminal.has(state.currentJob.status)) $("#progress-message").textContent = "实时连接已中断，浏览器正在自动重连…"; };
  }
  async function startBrowserDownload(job) {
    try {
      if (state.downloader && !["completed", "cancelled", "failed", "error"].includes(state.downloader.state)) {
        if (state.downloader.jobId === job.id) { openProgress(job); return; }
        throw Error("当前浏览器已有一个下载任务，请先暂停或取消后再启动其他任务");
      }
      if (["paused","failed"].includes(job.status)) job = await api(`/api/imagery/jobs/${encodeURIComponent(job.id)}/resume`, {method:"POST"});
      const downloader = new window.CanglingImageryDownloader({jobId:job.id, concurrency:4, pageSize:20}); state.downloader = downloader;
      downloader.addEventListener("jobprogress", event => updateProgress(event.detail.job));
      downloader.addEventListener("tileprogress", event => $("#progress-message").textContent = `浏览器正在处理第 ${event.detail.page} 批瓦片…`);
      downloader.addEventListener("warning", event => $("#progress-message").textContent = event.detail.message);
      downloader.addEventListener("error", event => { $("#progress-message").textContent = event.detail.error.message; toast(event.detail.error.message, true); });
      downloader.addEventListener("state", event => { if (event.detail.state === "completed") { toast("影像下载已完成"); loadJobs(); } });
      openProgress(job); downloader.start().catch(error => toast(error.message, true));
    } catch (error) { toast(error.message, true); }
  }
  async function jobAction(id, action) {
    let job = state.jobs.find(item => item.id === id) || await api(`/api/imagery/jobs/${encodeURIComponent(id)}`);
    if (action === "details") return openProgress(job);
    if (action === "download") return startBrowserDownload(job);
    if (action === "delete") {
      state.deletingJob = job;
      $("#delete-job-name").textContent = repositoryById(job.repository_id)?.name || job.repository_id;
      showError("#delete-job-error");
      $("#delete-job-dialog").showModal();
      return;
    }
    try {
      if (action === "takeover") { const result = await api(`/api/imagery/jobs/${encodeURIComponent(id)}/takeover`, {method:"POST"}); toast(`服务端已接管 ${result.queued_chunks} 个分片`); }
      else job = await api(`/api/imagery/jobs/${encodeURIComponent(id)}/${action}`, {method:"POST"});
      if (state.currentJob?.id === id && job?.id) updateProgress(job); await loadJobs();
    } catch (error) { toast(error.message, true); }
  }

  function switchTab(name) { document.querySelectorAll(".imagery-tab").forEach(button => button.classList.toggle("active", button.dataset.tab === name)); document.querySelectorAll("[data-panel]").forEach(panel => panel.classList.toggle("hidden", panel.dataset.panel !== name)); if (name === "jobs") loadJobs(); }
  document.querySelectorAll(".imagery-tab").forEach(button => button.onclick = () => switchTab(button.dataset.tab));
  $("#add-repository").onclick = () => openRepository(); $("#save-repository").onclick = saveRepository; $("#create-job").onclick = createJob;
  $("#repository-search").oninput = event => { clearTimeout(state.searchTimer); state.searchTimer = setTimeout(() => { state.repositoryQuery = event.target.value.trim(); state.repositoryPage = 1; loadRepositories(); }, 250); };
  $("#repository-rows").onclick = async event => {
    const copy = event.target.closest("[data-copy]");
    if (copy) {
      const copied = await copyText(copy.dataset.copy);
      toast(copied ? "已复制" : "复制失败，请手动选择文本", !copied);
      return;
    }
    const button = event.target.closest("[data-action]"); if (!button) return; const item = repositoryById(button.dataset.id); if (button.dataset.action === "job") openJob(item); if (button.dataset.action === "edit") openRepository(item); if (button.dataset.action === "delete") { state.deletingRepository = item; $("#delete-repository-name").textContent = item.name; showError("#delete-error"); $("#delete-dialog").showModal(); }
  };
  $("#confirm-delete").onclick = async () => { const button = $("#confirm-delete"); button.disabled = true; try { await api(`/api/imagery/repositories/${encodeURIComponent(state.deletingRepository.id)}`, {method:"DELETE"}); $("#delete-dialog").close(); toast("影像仓库已删除"); loadRepositories(); } catch (error) { showError("#delete-error", error.message); } finally { button.disabled = false; } };
  $("#confirm-delete-job").onclick = async () => {
    const button = $("#confirm-delete-job"), job = state.deletingJob;
    if (!job) return;
    button.disabled = true;
    try {
      if (state.downloader?.jobId === job.id) {
        await state.downloader.cancel().catch(() => {});
        state.downloader = null;
      }
      if (state.currentJob?.id === job.id) { state.events?.close(); state.events = null; }
      await api(`/api/imagery/jobs/${encodeURIComponent(job.id)}`, {method:"DELETE"});
      if (state.currentJob?.id === job.id) { state.currentJob = null; $("#progress-dialog").close(); }
      $("#delete-job-dialog").close(); state.deletingJob = null; toast("下载任务已删除"); await loadJobs();
    } catch (error) { showError("#delete-job-error", error.message); } finally { button.disabled = false; }
  };
  $("#job-rows").onclick = event => { const button = event.target.closest("[data-job-action]"); if (button) jobAction(button.dataset.id, button.dataset.jobAction); };
  $("#progress-pause").onclick = () => { if (!state.currentJob) return; const action = state.currentJob.status === "paused" ? "resume" : "pause"; if (state.downloader?.jobId === state.currentJob.id) { (action === "pause" ? state.downloader.pause() : state.downloader.resume()).catch(error => toast(error.message, true)); } else jobAction(state.currentJob.id, action); };
  $("#progress-cancel").onclick = () => { if (!state.currentJob) return; if (state.downloader?.jobId === state.currentJob.id) state.downloader.cancel().catch(error => toast(error.message, true)); else jobAction(state.currentJob.id, "cancel"); };
  $("#progress-takeover").onclick = () => state.currentJob && jobAction(state.currentJob.id, "takeover");
  $("#progress-dialog").addEventListener("close", () => { state.events?.close(); state.events = null; });
  document.querySelectorAll('dialog button[value="cancel"]').forEach(button => { button.type = "button"; button.onclick = () => button.closest("dialog").close("cancel"); });

  loadLeadership(); loadRepositories(); loadJobs();
  setInterval(() => { if (!document.hidden) { loadLeadership(); loadJobs(); } }, 5000);
})();
