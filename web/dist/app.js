/* ============================================================
   sing-box panel — live client
   只做三件事：调 REST、渲染、把 WebSocket 数据塞进环形缓冲。
   ============================================================ */

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];

const state = {
  token: localStorage.getItem('sb_token') || '',
  authRequired: true,
  status: null,
  engine: null,
  subs: [],
  nodes: [],
  delays: {},
  snapshots: [],
  configRaw: '',
  logs: [],
  conns: [],
  traffic: { up: 0, down: 0, upHist: [], downHist: [] },
  paused: false,
  page: 'dashboard',
  testRunning: false,
  // 运行时按需填充的接口数据（初始声明便于阅读与调试）
  tproxy: null, tun: null, lanproxy: null, routing: null, sysproxy: null,
  autostart: null, geo: null
};

let pollTimers = [];
let connFallbackTimer = null;

// 透明代理依赖 nftables，只在 Linux 上可用；
// 后端未上报 platform（旧版本）时按 Linux 处理，保持向后兼容。
function isLinux() {
  const p = state.status && state.status.platform;
  return !p || p === 'linux';
}

// 当前内核：sing-box（默认）或 mihomo。两个内核的配置格式、文件名、
// 校验命令都不同，前端所有相关文案都从这里取，避免页面里写死 sing-box。
function eng() { return (state.status && state.status.engine) || 'sing-box'; }
function engIsMihomo() { return eng() === 'mihomo'; }
function engFile() { return engIsMihomo() ? 'config.yaml' : 'config.json'; }
function engLang() { return engIsMihomo() ? 'YAML' : 'JSON'; }
function engCheckCmd() {
  return engIsMihomo() ? '$ mihomo -t -d <data> -f config.yaml' : '$ sing-box check -c config.json';
}
// 内核展示名（状态接口已带 engine_label，缺省时本地兜底）
function engLabel() { return (state.status && state.status.engine_label) || (engIsMihomo() ? 'Mihomo' : 'sing-box'); }
// TUN 虚拟网卡名，两个内核不同，用于文案与提示
function engTunDev() { return engIsMihomo() ? 'mihomo-tun' : 'singbox-tun'; }

/* ------------------------------------------------------------ api */

async function api(path, opts = {}) {
  const res = await fetch(path, {
    ...opts,
    headers: {
      'content-type': 'application/json',
      ...(state.token ? { authorization: `Bearer ${state.token}` } : {}),
      ...(opts.headers || {})
    }
  });
  if (res.status === 401) {
    logout();
    throw new Error('unauthorized');
  }
  const text = await res.text();
  let body = null;
  try { body = text ? JSON.parse(text) : null; } catch { body = { raw: text }; }
  if (!res.ok) {
    throw new Error((body && (body.message || body.error)) || res.statusText);
  }
  return body;
}

/* ------------------------------------------------------------ format */

function fmtBytes(b) {
  if (!b) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0, v = b;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v >= 100 ? 0 : 1)} ${u[i]}`;
}

function fmtRate(bps) {
  if (!bps) return '0 B/s';
  return fmtBytes(bps) + '/s';
}

function fmtUptime(sec) {
  if (!sec || sec < 0) return '未运行';
  const d = Math.floor(sec / 86400);
  const h = Math.floor((sec % 86400) / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = Math.floor(sec % 60);
  if (d > 0) return `已运行 ${d} 天 ${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  if (h > 0) return `已运行 ${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  return `已运行 ${m}:${String(s).padStart(2, '0')}`;
}

function fmtTime(iso) {
  if (!iso) return '—';
  const d = new Date(iso);
  if (isNaN(d)) return '—';
  const p = (n) => String(n).padStart(2, '0');
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function fmtAgo(iso) {
  if (!iso) return '—';
  const d = new Date(iso);
  const diff = (Date.now() - d.getTime()) / 1000;
  if (diff < 60) return '刚刚';
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`;
  return `${Math.floor(diff / 86400)} 天前`;
}

function fmtExpire(ts) {
  if (!ts) return '—';
  const d = new Date(ts * 1000);
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

// 名称前的国旗 emoji 在部分字体下会渲染成 "US" 字母，界面已有地区徽标，这里去掉
function stripFlag(s) {
  return String(s ?? '').replace(/^[\u{1F1E6}-\u{1F1FF}]{2}\s*/u, '').trim();
}

function esc(s) {
  return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

function latClass(v) {
  if (v === undefined || v === null || v < 0) return 'none';
  if (v < 80) return 'good';
  if (v < 150) return 'ok';
  if (v < 250) return 'mid';
  return 'bad';
}

function latText(v) {
  if (v === undefined || v === null) return '—';
  return v < 0 ? '超时' : v + ' ms';
}

function regionColor(code) {
  let h = 0;
  for (const ch of code || '??') h = (h * 31 + ch.charCodeAt(0)) % 360;
  return `linear-gradient(150deg, hsl(${h} 45% 38%), hsl(${h} 48% 24%))`;
}

/* ------------------------------------------------------------ toast / dialog */

function toast(kind, title, desc) {
  const el = document.createElement('div');
  el.className = `toast ${kind}`;
  const icon = kind === 'ok' ? 'i-check' : kind === 'err' ? 'i-alert' : 'i-info';
  el.innerHTML = `<svg class="ic" width="14" height="14"><use href="#${icon}"/></svg>
    <div><div class="t">${esc(title)}</div>${desc ? `<div class="d">${esc(desc)}</div>` : ''}</div>`;
  $('#toasts').appendChild(el);
  setTimeout(() => { el.classList.add('out'); setTimeout(() => el.remove(), 320); }, 3600);
}

let dialogResolve = null;

function confirmDialog({ title, body, confirm = '确认', danger = false }) {
  return new Promise((resolve) => {
    dialogResolve = resolve;
    $('#dialog').innerHTML = `
      <div class="dialog-hd">
        <h3>${danger ? '<svg width="15" height="15"><use href="#i-alert"/></svg>' : ''}${esc(title)}</h3>
        ${body ? `<p>${body}</p>` : ''}
      </div>
      <div class="dialog-ft">
        <button class="btn" id="dlgCancel">取消</button>
        <button class="btn ${danger ? 'danger' : 'primary'}" id="dlgOk">${esc(confirm)}</button>
      </div>`;
    $('#dialogOverlay').classList.add('open');
    $('#dlgCancel').onclick = () => closeDialog(false);
    $('#dlgOk').onclick = () => closeDialog(true);
  });
}

function closeDialog(val) {
  $('#dialogOverlay').classList.remove('open');
  if (dialogResolve) { dialogResolve(val); dialogResolve = null; }
  if (authDialogResolve) { authDialogResolve(val || null); authDialogResolve = null; }
}

let authDialogResolve = null;

function authCredDialog(defaultUser) {
  return new Promise((resolve) => {
    authDialogResolve = resolve;
    $('#dialog').innerHTML = `
      <div class="dialog-hd">
        <h3>启用登录</h3>
        <p>保存后其他已登录会话会失效。</p>
      </div>
      <div class="dialog-bd">
        <div class="field"><label>用户名</label><input class="input" id="authUserInput" value="${esc(defaultUser || 'admin')}" autocomplete="off" /></div>
        <div class="field"><label>新密码</label><input class="input" id="authPassInput" type="password" placeholder="至少 8 位" autocomplete="new-password" /></div>
      </div>
      <div class="dialog-ft">
        <button class="btn" id="dlgCancel">取消</button>
        <button class="btn primary" id="dlgOk">启用登录</button>
      </div>`;
    $('#dialogOverlay').classList.add('open');
    setTimeout(() => $('#authPassInput')?.focus(), 60);
    $('#dlgCancel').onclick = () => closeDialog(false);
    $('#dlgOk').onclick = () => {
      const username = $('#authUserInput').value.trim();
      const password = $('#authPassInput').value;
      if (!username) { toast('err', '请填写用户名'); return; }
      if (password.length < 8) { toast('err', '密码至少 8 位'); return; }
      closeDialog({ username, password });
    };
    $('#authPassInput').onkeydown = (e) => {
      if (e.key === 'Enter') $('#dlgOk').click();
    };
  });
}

$('#dialogOverlay').addEventListener('click', (e) => {
  if (e.target.id === 'dialogOverlay') closeDialog(false);
});

/* ------------------------------------------------------------ auth */

async function login(user, pass) {
  const res = await fetch('/api/v1/auth/login', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ username: user, password: pass })
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(body.message || '登录失败');
  state.token = body.token;
  localStorage.setItem('sb_token', body.token);
}

function showLogin() {
  state.setupMode = false;
  $('#app').style.display = 'none';
  $('#login').classList.add('open');
  $('#loginBtn').textContent = '登录';
  $('#loginPass2Field').style.display = 'none';
  $('#loginErr').textContent = '';
  setTimeout(() => $('#loginPass')?.focus(), 200);
}

// 首次启动初始化：复用登录页，多一个确认密码框，提交走 /auth/setup
function showSetup() {
  state.setupMode = true;
  $('#app').style.display = 'none';
  $('#login').classList.add('open');
  $('#loginBtn').textContent = '完成初始化';
  $('#loginPass2Field').style.display = '';
  $('#loginErr').textContent = '';
  setTimeout(() => $('#loginUser')?.focus(), 200);
}

function logout() {
  // 免登录模式下没有可退回的登录页，退出按钮已隐藏。
  if (!state.authRequired) return;
  state.token = '';
  localStorage.removeItem('sb_token');
  pollTimers.forEach(clearInterval);
  pollTimers = [];
  if (connFallbackTimer) { clearInterval(connFallbackTimer); connFallbackTimer = null; }
  for (const ws of [trafficWS, connWS]) {
    if (!ws) continue;
    ws.onclose = null;
    ws.close();
  }
  trafficWS = null;
  connWS = null;
  connRowMap.clear();
  showLogin();
}

$('#loginForm').addEventListener('submit', async (e) => {
  e.preventDefault();
  const btn = $('#loginBtn');
  btn.disabled = true;
  $('#loginErr').textContent = '';
  try {
    if (state.setupMode) {
      const user = $('#loginUser').value.trim();
      const pass = $('#loginPass').value;
      if (!user) { $('#loginErr').textContent = '请填写用户名'; return; }
      if (pass.length < 8) { $('#loginErr').textContent = '密码至少 8 位'; return; }
      if (pass !== $('#loginPass2').value) { $('#loginErr').textContent = '两次输入的密码不一致'; return; }
      await api('/api/v1/auth/setup', { method: 'POST', body: JSON.stringify({ username: user, password: pass }) });
      state.setupMode = false;
      state.needsSetup = false;
      await login(user, pass);
    } else {
      await login($('#loginUser').value.trim(), $('#loginPass').value);
    }
    await boot();
  } catch (err) {
    $('#loginErr').textContent = err.message;
  } finally {
    btn.disabled = false;
  }
});

$('#logoutBtn').addEventListener('click', logout);

/* ------------------------------------------------------------ boot / polling */

async function boot() {
  try {
    const st = await api('/api/v1/status');
    state.status = st;
  } catch (e) {
    return;
  }
  $('#login').classList.remove('open');
  $('#app').style.display = '';
  await Promise.all([loadSubs(), loadNodes(), loadConfig(), loadLogs()]);
  renderAll();
  connectTraffic();
  connectConnections();
  loadSysproxy();
  loadTproxy();
  loadLanProxy();
  loadTun();
  loadExitIp();
  loadAutostart();
  loadRouting();
  loadEngine();
  pollTimers.forEach(clearInterval);
  pollTimers = [setInterval(pollStatus, 5000), setInterval(pollLogs, 3000)];
}

async function pollStatus() {
  try {
    state.status = await api('/api/v1/status');
    renderStatus();
    loadTun();
    loadSysproxy();
    loadExitIp();
    if (state.lanproxy) renderLanProxyStats(state.lanproxy);
  } catch (e) { /* ignore transient */ }
}

async function loadSubs() {
  const d = await api('/api/v1/subscriptions');
  state.subs = d.subscriptions || [];
}

async function loadNodes() {
  const d = await api('/api/v1/nodes');
  state.nodes = d.nodes || [];
  state.delays = d.delays || {};
}

async function loadConfig() {
  const d = await api('/api/v1/config');
  state.configRaw = d.raw || '';
  state.snapshots = d.snapshots || [];
}

async function loadLogs() {
  const d = await api('/api/v1/logs');
  state.logs = d.lines || [];
  renderLogs();
}

// 日志只在「看得到日志框」的页面里轮询：/logs 每次最多返回 400 行，
// 在别的页面、或标签页切到后台时继续拉，纯粹是给 GC 制造垃圾。
function logsVisible() {
  if (document.visibilityState !== 'visible') return false;
  return state.page === 'dashboard' || state.page === 'service';
}

function pollLogs() {
  if (logsVisible()) loadLogs();
}

/* ------------------------------------------------------------ render: status */

function renderStatus() {
  const s = state.status;
  if (!s) return;
  const running = s.running;
  const chip = $('#topStatus');
  chip.className = 'chip ' + (running ? 'ok' : 'warn');
  chip.innerHTML = `<i class="dot ${running ? 'live' : ''}"></i> ${running ? '核心运行中' : '核心已停止'}`;

  $('#topVersion').style.display = s.version ? '' : 'none';
  $('#topVersion').innerHTML = `<svg width="12" height="12"><use href="#i-cpu"/></svg> ${esc(s.version || '')}`;

  $('#heroStatus').textContent = running ? '运行中' : '已停止';
  $('#heroGlyph').style.background = running ? 'var(--lime-soft)' : 'rgba(255,111,97,.1)';
  $('#heroGlyph').style.color = running ? 'var(--lime)' : 'var(--red)';
  $('#heroGlyph').style.borderColor = running ? 'var(--lime-line)' : 'rgba(255,111,97,.3)';
  $('#heroUptime').textContent = running ? fmtUptime(s.uptime) : '未运行';
  $('#heroPid').textContent = s.pid ? `PID ${s.pid}` : 'PID —';
  $('#heroCheck').textContent = s.last_check_at
    ? (s.last_check_ok ? `校验通过 · ${fmtAgo(s.last_check_at)}` : `校验失败 · ${fmtAgo(s.last_check_at)}`)
    : (s.last_check ? (s.last_check_ok ? '上次校验通过' : '上次校验失败') : '尚未校验');
  $('#heroNodes').textContent = s.enabled;
  $('#heroRules').textContent = s.rules;
  const mixedPort = s.mixed_port || 7890;
  $('#heroIn').textContent = `mixed :${mixedPort}`;

  // memory（首页的内存环已移除，只保留设置页与「服务」页的两处数字）
  const panelMB = (s.panel_rss || 0) / 1048576;
  const coreMB = (s.rss || 0) / 1048576;
  $('#devMem').textContent = `面板 ${panelMB.toFixed(1)} / 20 MB`;
  $('#devMemBar').style.width = Math.min(100, (panelMB / 20) * 100) + '%';

  // stats
  $('#statNodes').innerHTML = `${s.enabled}<small>/ ${s.nodes}</small>`;
  $('#statNodesFoot').textContent = s.nodes - s.enabled > 0 ? `${s.nodes - s.enabled} 个已禁用` : '';
  $('#statRules').textContent = s.rules;

  // service page
  $('#svcState').textContent = running ? 'running' : 'stopped';
  $('#svcDot').style.color = running ? 'var(--lime)' : 'var(--red)';
  $('#svcDot').className = 'dot ' + (running ? 'live' : '');
  $('#svcPid').textContent = s.pid ? `PID ${s.pid}` : 'PID —';
  $('#svcUptime').textContent = running ? fmtUptime(s.uptime) : (s.last_error || '未运行');
  $('#svcRss').innerHTML = `${coreMB.toFixed(1)}<small> MB</small>`;
  $('#svcMemBar').style.width = Math.min(100, coreMB / 2) + '%';
  $('#svcMemNote').textContent = `面板 ${panelMB.toFixed(1)} MB · 核心 ${coreMB.toFixed(0)} MB`;
  $('#svcCfgVer').textContent = `v${s.config_version}`;
  $('#svcCheckBadge').textContent = s.last_check_ok ? '校验通过' : (s.last_check ? '校验失败' : '未校验');
  $('#svcCheckBadge').className = 'badge ' + (s.last_check_ok ? 'lime' : (s.last_check ? 'red' : ''));
  $('#svcCheckNote').textContent = s.last_check ? s.last_check.split('\n').slice(-1)[0].slice(0, 60) : '—';

  // settings
  const listenInput = $('#setListenInput');
  if (document.activeElement !== listenInput) listenInput.value = s.listen;
  const listenHost = s.listen.startsWith('[')
    ? s.listen.slice(1, s.listen.indexOf(']'))
    : s.listen.split(':')[0];
  const loopback = listenHost === '127.0.0.1' || listenHost === '::1' || listenHost === 'localhost';
  const listenNote = $('#setListenNote');
  // 只在有话要说时出现：启动参数固定了监听地址这一事实会影响面板内修改的效果
  listenNote.style.display = s.listen_pinned ? '' : 'none';
  listenNote.textContent = s.listen_pinned
    ? '启动参数 -listen 已固定监听地址，重启后以该参数为准'
    : '';
  listenNote.style.color = s.listen_pinned ? 'var(--amber)' : '';
  $('#setListen2').textContent = listenHost || '0.0.0.0';
  $('#listenWarn').style.display = loopback ? 'none' : '';
  $('#setDataDir').textContent = s.data_dir || '—';
  $('#setDataDir').title = s.data_dir || '';
  $('#setKeep').textContent = s.keep_snapshots ?? 3;
  $('#setWorkers').textContent = s.delay_workers ?? 3;
  const authOff = !s.login_required;
  state.authRequired = s.login_required;
  $('#logoutBtn').style.display = s.login_required ? '' : 'none';
  $('#setAuthState').textContent = authOff ? '免密码访问' : '需要登录';
  $('#setAuthState').className = 'badge ' + (authOff ? 'red' : 'lime');
  $('#setAuthUser').textContent = s.username || '—';
  $('#authSwitch').classList.toggle('on', authOff);
  $('#authWarn').style.display = authOff ? '' : 'none';
  $('#authListen').textContent = s.listen || '—';
  $('#setSbVer').textContent = s.version || '—';
  const el = $('#setEngineLabel'); if (el) el.textContent = eng();
  const ac = $('#asCoreLabel'); if (ac) ac.textContent = eng();
  const sf = $('#setStoreFmt'); if (sf) sf.textContent = `${engLang()} + rename`;
  $('#setPanelVer').textContent = s.panel_version ? `v${s.panel_version}` : '—';
  $('#setPlatform').textContent = s.arch || '—';
  $('#paletteVer').textContent = s.panel_version ? `sing-box panel v${s.panel_version}` : 'sing-box panel';
  $('#workersChip').textContent = `并发上限 ${s.delay_workers ?? 3}`;
  $('#tpCore').textContent = running ? '运行中' : '未运行';
  $('#tpMode').textContent = s.tproxy || 'redirect';

  // nav counts
  $('#navSubs').textContent = s.subs;
  $('#navNodes').textContent = s.nodes;

  // 网络页双平台复用：Linux=透明代理（redirect）+TUN，Windows=TUN 模式
  const linux = isLinux();
  $$('.nav-item[data-page="tproxy"]').forEach((el) => (el.style.display = ''));
  $$('[data-goto="tproxy"]').forEach((el) => (el.style.display = ''));
  $('#navTpLabel').textContent = linux ? '透明代理' : '代理模式';
  $('#dashTpLabel').textContent = linux ? '透明代理' : '代理模式';
  $('#tpTitle').textContent = linux ? '透明代理' : '代理模式';
  // 流量接管卡：redirect 行仅 Linux 显示，TUN 行两平台都有
  $('#redirectRow').style.display = linux ? '' : 'none';
  $('#redirectDivider').style.display = linux ? '' : 'none';
  $('#sysproxyRow').style.display = linux ? 'none' : '';
  $('#nftPreview').style.display = linux ? '' : 'none';
  // Windows 下右列（nftables 预览）不存在，网格退化为单列避免大片留白
  $('#page-tproxy .grid-2').style.gridTemplateColumns = linux ? '' : '1fr';

  // traffic stat
  renderTrafficStats();
}

/* ------------------------------------------------------------ render: subs */

function renderSubs() {
  const grid = $('#subGrid');
  if (!state.subs.length) {
    grid.innerHTML = `
      <button class="sub-add" id="emptyAddSub">
        <span class="plus"><svg width="14" height="14"><use href="#i-plus"/></svg></span>
        添加订阅
      </button>`;
    $('#emptyAddSub').onclick = openAddSub;
    return;
  }

  const tones = ['lime', 'teal', 'violet'];
  grid.innerHTML = state.subs.map((s, i) => {
    const pct = s.usage_total ? Math.min(100, (s.usage_used / s.usage_total) * 100) : 0;
    const hours = s.interval_hours > 0 ? s.interval_hours : 24;
    return `<div class="card sub-card" data-sub="${s.id}" title="点击编辑订阅">
      <div class="sub-head">
        <div class="sub-avatar ${tones[i % 3]}">${esc(s.name.slice(0, 1).toUpperCase())}</div>
        <div class="sub-id">
          <div class="sub-title">${esc(s.name)}</div>
          <div class="sub-url" title="${esc(s.url)}">${esc(s.url)}</div>
        </div>
        <span class="badge ${s.auto ? 'lime' : ''}">${s.auto ? `自动 · ${hours}h` : '手动'}</span>
      </div>
      <div class="usage">
        <div class="usage-bar"><i style="width:${pct}%"></i></div>
        <div class="usage-meta"><span>${fmtBytes(s.usage_used)} / ${fmtBytes(s.usage_total)}</span><span>${pct.toFixed(0)}%</span></div>
      </div>
      <div class="sub-stats">
        <div><div class="k">节点</div><div class="v">${s.node_count}</div></div>
        <div><div class="k">到期</div><div class="v">${fmtExpire(s.expire)}</div></div>
        <div><div class="k">上次更新</div><div class="v">${fmtAgo(s.updated_at)}</div></div>
      </div>
      <div class="sub-schedule">
        <div>
          <button class="switch sm ${s.auto ? 'on' : ''}" data-auto="${s.id}" role="switch" aria-checked="${!!s.auto}" title="${s.auto ? '关闭自动更新' : '开启自动更新'}"></button>
          <span class="k">自动更新</span>
        </div>
        <div>
          <span class="k">间隔</span>
          <input class="input sub-hours" type="number" min="1" max="720" step="1" value="${hours}" data-hours="${s.id}" ${s.auto ? '' : 'disabled'} />
          <span class="k">小时</span>
          <button class="btn ghost xs" data-save-hours="${s.id}" ${s.auto ? '' : 'disabled'}>保存</button>
        </div>
      </div>
      <div class="sub-actions">
        <button class="btn sm grow" data-refresh="${s.id}"><svg width="11" height="11"><use href="#i-refresh"/></svg>立即更新</button>
        <button class="btn sm ghost" data-del="${s.id}" title="删除" aria-label="删除"><svg width="11" height="11"><use href="#i-trash"/></svg></button>
      </div>
      ${s.last_error ? `<div class="callout danger callout-sm"><svg width="13" height="13"><use href="#i-alert"/></svg><div>${esc(s.last_error)}</div></div>` : ''}
    </div>`;
  }).join('');

  $$('[data-refresh]').forEach((b) => (b.onclick = () => refreshSub(b.dataset.refresh, b)));
  $$('[data-del]').forEach((b) => (b.onclick = () => deleteSub(b.dataset.del)));
  // 单击订阅卡片弹出编辑（按钮 / 开关 / 输入框不触发）
  $$('.sub-card[data-sub]').forEach((card) => {
    card.onclick = (e) => {
      if (e.target.closest('button, input')) return;
      openEditSub(card.dataset.sub);
    };
  });
  $$('[data-auto]').forEach((b) => (b.onclick = () => setSubAuto(b.dataset.auto, !b.classList.contains('on'))));
  $$('[data-save-hours]').forEach((b) => (b.onclick = () => setSubHours(b.dataset.saveHours)));
  $$('[data-hours]').forEach((inp) => (inp.onkeydown = (e) => {
    if (e.key === 'Enter') setSubHours(inp.dataset.hours);
  }));

  const total = state.subs.reduce((a, s) => a + s.node_count, 0);
  const used = state.subs.reduce((a, s) => a + (s.usage_used || 0), 0);
  const quota = state.subs.reduce((a, s) => a + (s.usage_total || 0), 0);
  $('#sumSubs').innerHTML = `${state.subs.length} <small>条</small>`;
  $('#sumNodes').innerHTML = `${total} <small>个</small>`;
  $('#sumUsed').innerHTML = `${fmtBytes(used)} <small>/ ${fmtBytes(quota)}</small>`;
  const autoN = state.subs.filter((s) => s.auto).length;
  $('#sumAuto').innerHTML = `${autoN} <small>条 · 各自间隔</small>`;
}

async function updateSubSettings(id, patch, okMsg) {
  try {
    await api(`/api/v1/subscriptions/${id}`, { method: 'PUT', body: JSON.stringify(patch) });
    toast('ok', '已保存', okMsg);
    await loadSubs();
    renderSubs();
  } catch (e) {
    toast('err', '保存失败', e.message);
    await loadSubs();
    renderSubs();
  }
}

async function setSubAuto(id, next) {
  await updateSubSettings(id, { auto: next }, next ? '已开启自动更新' : '已改为手动更新');
}

async function setSubHours(id) {
  const input = document.querySelector(`[data-hours="${id}"]`);
  const hours = parseInt(input.value, 10);
  if (!Number.isFinite(hours) || hours < 1 || hours > 720) {
    toast('err', '间隔无效', '请输入 1-720 小时');
    return;
  }
  await updateSubSettings(id, { interval_hours: hours }, `自动更新间隔已设为 ${hours} 小时`);
}

async function refreshSub(id, btn) {
  if (btn) { btn.disabled = true; btn.textContent = '更新中…'; }
  try {
    const d = await api(`/api/v1/subscriptions/${id}/refresh`, { method: 'POST' });
    toast('ok', '更新完成', `解析出 ${d.nodes} 个节点`);
    await Promise.all([loadSubs(), loadNodes()]);
    renderSubs(); renderNodes(); renderStatus();
  } catch (e) {
    toast('err', '更新失败', e.message);
  } finally {
    if (btn) { btn.disabled = false; }
  }
}

async function deleteSub(id) {
  const ok = await confirmDialog({
    title: '删除这个订阅？',
    confirm: '删除订阅',
    danger: true
  });
  if (!ok) return;
  try {
    await api(`/api/v1/subscriptions/${id}`, { method: 'DELETE' });
    toast('info', '订阅已删除');
    await Promise.all([loadSubs(), loadNodes()]);
    renderSubs(); renderNodes(); renderStatus();
  } catch (e) {
    toast('err', '删除失败', e.message);
  }
}

function openAddSub() {
  $('#dialog').innerHTML = `
    <div class="dialog-hd">
      <h3>新增订阅</h3>
    </div>
    <div class="dialog-bd">
      <div class="field"><label>订阅名称</label><input class="input" id="subName" placeholder="例如：主订阅" /></div>
      <div class="field"><label>订阅链接</label><input class="input" id="subUrl" placeholder="https://…" /></div>
      <div class="field">
        <label>自动更新</label>
        <div class="form-switch">
          <button type="button" class="switch sm on" id="subAuto" role="switch" aria-checked="true"></button>
          <span class="k">每 <input class="input sub-hours" id="subHours" type="number" min="1" max="720" step="1" value="24" /> 小时</span>
        </div>
      </div>
    </div>
    <div class="dialog-ft">
      <button class="btn" onclick="closeDialog(false)">取消</button>
      <button class="btn primary" id="subOk">拉取并解析</button>
    </div>`;
  $('#dialogOverlay').classList.add('open');
  const autoBtn = $('#subAuto');
  const hoursInput = $('#subHours');
  autoBtn.onclick = () => {
    const on = autoBtn.classList.toggle('on');
    autoBtn.setAttribute('aria-checked', String(on));
    hoursInput.disabled = !on;
  };
  $('#subOk').onclick = async () => {
    const name = $('#subName').value.trim();
    const url = $('#subUrl').value.trim();
    if (!url) { toast('err', '缺少订阅链接'); return; }
    const auto = autoBtn.classList.contains('on');
    const hours = parseInt(hoursInput.value, 10);
    if (!Number.isFinite(hours) || hours < 1 || hours > 720) {
      toast('err', '间隔无效', '请输入 1-720 小时');
      return;
    }
    $('#subOk').disabled = true;
    $('#subOk').textContent = '拉取中…';
    try {
      const d = await api('/api/v1/subscriptions', { method: 'POST', body: JSON.stringify({ name, url, auto, interval_hours: hours }) });
      closeDialog(true);
      toast('ok', '订阅已添加', `解析出 ${d.nodes} 个节点`);
      await Promise.all([loadSubs(), loadNodes()]);
      renderSubs(); renderNodes(); renderStatus();
    } catch (e) {
      toast('err', '添加失败', e.message);
      $('#subOk').disabled = false;
      $('#subOk').textContent = '拉取并解析';
    }
  };
}

function openEditSub(id) {
  const s = state.subs.find((x) => x.id === id);
  if (!s) return;
  $('#dialog').innerHTML = `
    <div class="dialog-hd">
      <h3>编辑订阅</h3>
    </div>
    <div class="dialog-bd">
      <div class="field"><label>订阅名称</label><input class="input" id="subName" value="${esc(s.name)}" placeholder="例如：主订阅" /></div>
      <div class="field"><label>订阅链接</label><input class="input" id="subUrl" value="${esc(s.url)}" placeholder="https://…" /></div>
      <div class="field">
        <label>自动更新</label>
        <div class="form-switch">
          <button type="button" class="switch sm ${s.auto ? 'on' : ''}" id="subAuto" role="switch" aria-checked="${!!s.auto}"></button>
          <span class="k">每 <input class="input sub-hours" id="subHours" type="number" min="1" max="720" step="1" value="${s.interval_hours > 0 ? s.interval_hours : 24}" ${s.auto ? '' : 'disabled'} /> 小时</span>
        </div>
      </div>
      <div class="field"><span class="k">修改链接后保存会立即重新拉取该订阅的节点</span></div>
    </div>
    <div class="dialog-ft">
      <button class="btn" onclick="closeDialog(false)">取消</button>
      <button class="btn primary" id="subOk">保存修改</button>
    </div>`;
  $('#dialogOverlay').classList.add('open');
  const autoBtn = $('#subAuto');
  const hoursInput = $('#subHours');
  autoBtn.onclick = () => {
    const on = autoBtn.classList.toggle('on');
    autoBtn.setAttribute('aria-checked', String(on));
    hoursInput.disabled = !on;
  };
  $('#subOk').onclick = async () => {
    const name = $('#subName').value.trim();
    const url = $('#subUrl').value.trim();
    if (!url) { toast('err', '缺少订阅链接'); return; }
    const auto = autoBtn.classList.contains('on');
    const hours = parseInt(hoursInput.value, 10);
    if (!Number.isFinite(hours) || hours < 1 || hours > 720) {
      toast('err', '间隔无效', '请输入 1-720 小时');
      return;
    }
    const patch = {};
    if (name && name !== s.name) patch.name = name;
    if (url !== s.url) patch.url = url;
    if (auto !== s.auto) patch.auto = auto;
    if (auto && hours !== s.interval_hours) patch.interval_hours = hours;
    if (!Object.keys(patch).length) { closeDialog(false); return; }
    const urlChanged = 'url' in patch;
    $('#subOk').disabled = true;
    $('#subOk').textContent = '保存中…';
    try {
      await api(`/api/v1/subscriptions/${id}`, { method: 'PUT', body: JSON.stringify(patch) });
      closeDialog(true);
      if (urlChanged) {
        toast('ok', '已保存', '链接已修改，正在重新拉取节点…');
        await refreshSub(id);
      } else {
        toast('ok', '已保存', '订阅设置已更新');
        await loadSubs();
        renderSubs();
      }
    } catch (e) {
      toast('err', '保存失败', e.message);
      $('#subOk').disabled = false;
      $('#subOk').textContent = '保存修改';
    }
  };
}

/* ------------------------------------------------------------ render: nodes */

let nodeFilter = { group: 'all', q: '' };

function filteredNodes() {
  return state.nodes.filter((n) => {
    if (nodeFilter.group !== 'all' && n.region !== nodeFilter.group) return false;
    if (nodeFilter.q) {
      const q = nodeFilter.q.toLowerCase();
      return n.name.toLowerCase().includes(q) || n.type.includes(q) || n.server.toLowerCase().includes(q);
    }
    return true;
  });
}

function renderNodes() {
  // region tabs
  const groups = {};
  state.nodes.forEach((n) => { const r = n.region || 'OTHER'; groups[r] = (groups[r] || 0) + 1; });
  const tabs = [`<button class="${nodeFilter.group === 'all' ? 'on' : ''}" data-group="all">全部 ${state.nodes.length}</button>`]
    .concat(Object.entries(groups).sort((a, b) => b[1] - a[1]).map(([r, c]) =>
      `<button class="${nodeFilter.group === r ? 'on' : ''}" data-group="${r}">${r} ${c}</button>`));
  $('#nodeGroups').innerHTML = tabs.join('');
  $$('#nodeGroups button').forEach((b) => (b.onclick = () => {
    nodeFilter.group = b.dataset.group;
    renderNodes();
  }));

  const rows = filteredNodes();
  const tbody = $('#nodeTable tbody');
  if (!rows.length) {
    tbody.innerHTML = `<tr><td colspan="6" class="empty-cell">
      ${state.nodes.length ? '没有匹配的节点' : '还没有节点，先去「订阅」页添加订阅'}</td></tr>`;
  } else {
    tbody.innerHTML = rows.map((n) => {
      const d = state.delays[n.tag];
      const ms = d ? d.ms : undefined;
      const c = latClass(ms);
      return `<tr>
        <td><div class="cell-main">
          <span class="flag" style="background:${regionColor(n.region)}">${esc(n.region || '??')}</span>
          <div class="col"><div class="t">${esc(stripFlag(n.name))}</div><div class="s">${esc(n.sub_id.slice(-6))}</div></div>
        </div></td>
        <td><span class="badge mono p-${esc(n.type)}">${esc(n.type)}</span></td>
        <td><div class="cell-addr">${esc(n.server)}:${n.port}</div></td>
        <td><span class="lat ${c}">${latText(ms)}</span></td>
        <td><button class="switch sm ${n.enabled ? 'on' : ''}" data-toggle="${n.id}" role="switch" aria-checked="${!!n.enabled}"></button></td>
        <td class="t-right"><button class="btn ghost xs" data-ping="${esc(n.tag)}">测速</button></td>
      </tr>`;
    }).join('');
  }

  $$('[data-toggle]').forEach((b) => (b.onclick = () => toggleNode(b.dataset.toggle, b)));
  $$('[data-ping]').forEach((b) => (b.onclick = () => testOne(b.dataset.ping)));

  const enabled = state.nodes.filter((n) => n.enabled).length;
  $('#nodeFoot').textContent = `显示 ${rows.length} 个节点 · 已启用 ${enabled} 个`;
}

async function toggleNode(id, btn) {
  const node = state.nodes.find((n) => n.id === id);
  if (!node) return;
  const next = !node.enabled;
  try {
    await api(`/api/v1/nodes/${id}`, { method: 'PUT', body: JSON.stringify({ enabled: next }) });
    node.enabled = next;
    btn.classList.toggle('on', next);
    btn.setAttribute('aria-checked', String(next));
    toast(next ? 'ok' : 'info', next ? '节点已启用' : '节点已禁用', node.name);
    await pollStatus();
  } catch (e) {
    toast('err', '操作失败', e.message);
  }
}

async function testOne(tag) {
  if (!state.status?.running) { toast('err', '核心未运行', `先到服务页启动 ${eng()}`); return; }
  try {
    // 单点测速是显式动作，永远实测，不吃缓存。
    const d = await api('/api/v1/nodes/delay', { method: 'POST', body: JSON.stringify({ tags: [tag], refresh: true }) });
    state.delays = d.delays;
    renderNodes(); renderDashNodes(); renderStatus();
  } catch (e) {
    toast('err', '测速失败', e.message);
  }
}

async function testAll() {
  if (state.testRunning) return;
  if (!state.status?.running) { toast('err', '核心未运行', `先到服务页启动 ${eng()}`); return; }
  state.testRunning = true;
  $('#testAllBtn').disabled = true;
  $('#stProgress').textContent = '测速中…';
  try {
    const tags = filteredNodes().map((n) => n.tag);
    const refresh = !!$('#forceTest')?.checked;
    const d = await api('/api/v1/nodes/delay', { method: 'POST', body: JSON.stringify({ tags, refresh }) });
    state.delays = d.delays;
    const cached = d.cached || 0;
    $('#stProgress').textContent = `完成 ${tags.length}/${tags.length}` + (cached ? ` · ${cached} 缓存` : '');
    renderNodes(); renderDashNodes(); renderStatus();
    toast('ok', '测速完成', cached ? `实测 ${tags.length - cached} 个，${cached} 个命中缓存` : `${tags.length} 个节点全部实测`);
  } catch (e) {
    toast('err', '测速失败', e.message);
  } finally {
    state.testRunning = false;
    $('#testAllBtn').disabled = false;
  }
}

/* ------------------------------------------------------------ render: dashboard nodes */

function renderDashNodes() {
  const box = $('#dashNodes');
  if (!state.nodes.length) {
    box.innerHTML = `<div class="empty-cell">还没有节点，先去「订阅」页添加订阅</div>`;
    return;
  }
  const withDelay = state.nodes
    .map((n) => ({ n, ms: state.delays[n.tag]?.ms }))
    .sort((a, b) => {
      const av = a.ms === undefined || a.ms < 0 ? 1e9 : a.ms;
      const bv = b.ms === undefined || b.ms < 0 ? 1e9 : b.ms;
      return av - bv;
    })
    .slice(0, 6);

  box.innerHTML = withDelay.map(({ n, ms }) => {
    const c = latClass(ms);
    return `<div class="node-mini">
      <span class="flag" style="background:${regionColor(n.region)}">${esc(n.region || '??')}</span>
      <span class="nm">${esc(stripFlag(n.name))}<em>${esc(n.type)}</em></span>
      <span class="lat lat-right ${c}">${latText(ms)}</span>
    </div>`;
  }).join('');
}

/* ------------------------------------------------------------ render: config */

function renderConfig() {
  const editor = $('#cfgEditor');
  const raw = state.configRaw || '';
  if (editor) editor.value = raw;
  $('#codeStat').textContent = raw ? `行 ${raw.split('\n').length}` : '空';
  const fn = $('#cfgFileName'); if (fn) fn.textContent = engFile();
  const tab = $('#cfgTabName'); if (tab) tab.textContent = `原始 ${engLang()}`;
  const chk = $('#checkCmd'); if (chk) chk.textContent = engCheckCmd();

  const list = $('#snapList');
  if (!state.snapshots.length) {
    list.innerHTML = `<div class="empty-cell">还没有快照，第一次下发后自动生成</div>`;
  } else {
    list.innerHTML = state.snapshots.map((s) => `
      <div class="snap">
        <div class="v">#${s.version}</div>
        <div class="meta">
          <div class="t">${esc(s.note)}</div>
          <div class="d">${fmtTime(s.time)} · ${(s.size / 1024).toFixed(1)} KB</div>
        </div>
        <button class="btn ghost xs" data-rollback="${s.version}">回滚</button>
      </div>`).join('');
    $$('[data-rollback]').forEach((b) => (b.onclick = () => rollbackTo(+b.dataset.rollback)));
  }
  $('#snapCount').textContent = `保留 ${state.snapshots.length}/${state.status?.keep_snapshots ?? 3}`;
}

async function renderFromTemplate() {
  try {
    const d = await api('/api/v1/config/render', { method: 'POST' });
    state.configRaw = d.raw;
    renderConfig();
    toast('ok', '已重新渲染');
  } catch (e) {
    toast('err', '渲染失败', e.message);
  }
}

async function validateConfig() {
  const raw = $('#cfgEditor')?.value || state.configRaw;
  const out = $('#checkOut');
  out.innerHTML = `<div class="dim">${esc(engCheckCmd())}</div><div class="dim">  parsing…</div>`;
  try {
    const r = await api('/api/v1/config/validate', { method: 'POST', body: JSON.stringify({ raw }) });
    out.innerHTML = `<div class="dim">${esc(engCheckCmd())}</div>
      <div class="${r.ok ? 'ok' : 'err'}">${r.ok ? '✓ 校验通过' : '✗ 校验失败'}（${r.elapsed_ms}ms）</div>
      ${r.output ? `<div class="${r.ok ? 'dim' : 'err'}">${esc(r.output).replace(/\n/g, '<br>')}</div>` : ''}`;
    toast(r.ok ? 'ok' : 'err', r.ok ? '校验通过' : '校验失败', r.ok ? '' : '配置未写入磁盘');
  } catch (e) {
    out.innerHTML = `<div class="err">${esc(e.message)}</div>`;
    toast('err', '校验失败', e.message);
  }
}

async function applyConfig() {
  const raw = $('#cfgEditor')?.value || state.configRaw;
  if (!raw) { toast('err', '没有配置内容'); return; }
  const ok = await confirmDialog({
    title: '保存并应用配置？',
    body: '校验通过后写入并重启核心，失败自动回滚。',
    confirm: '保存并应用'
  });
  if (!ok) return;
  try {
    const r = await api('/api/v1/config/apply', {
      method: 'POST',
      body: JSON.stringify({ raw, note: '面板手动保存' })
    });
    state.snapshots = r.snapshots || [];
    toast('ok', '配置已应用', '核心已重启并通过探活');
    await pollStatus();
    renderConfig();
  } catch (e) {
    toast('err', '应用失败', e.message);
    $('#checkOut').innerHTML = `<div class="err">${esc(e.message)}</div>`;
  }
}

async function rollbackTo(version) {
  const ok = await confirmDialog({
    title: `回滚到 v${version}？`,
    body: '校验通过后写入并重启核心。',
    confirm: '回滚并重启',
    danger: true
  });
  if (!ok) return;
  try {
    await api('/api/v1/config/rollback', { method: 'POST', body: JSON.stringify({ version }) });
    toast('ok', '已回滚', `配置已恢复到 v${version}`);
    await Promise.all([loadConfig(), pollStatus()]);
    renderConfig();
  } catch (e) {
    toast('err', '回滚失败', e.message);
  }
}

/* ------------------------------------------------------------ render: logs */

function logLine(l, fresh) {
  const t = new Date(l.at);
  const ts = isNaN(t) ? '--:--:--' : t.toTimeString().slice(0, 8);
  // sing-box 的行前缀是 "+0800 2026-10-20 02:13:46 INFO[0000] "
  let raw = String(l.raw || '')
    .replace(/^[+-]\d{4}\s+\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}\s+/, '')
    .replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '');
  let lv = 'info';
  const m = raw.match(/^(INFO|WARN|ERROR|DEBUG|FATAL|TRACE)\[\d+\]\s*/);
  if (m) {
    lv = m[1].toLowerCase() === 'fatal' ? 'error' : m[1].toLowerCase();
    raw = raw.slice(m[0].length);
  } else if (/\b(ERROR|FATAL)\b/i.test(raw)) lv = 'error';
  else if (/\bWARN\b/i.test(raw)) lv = 'warn';
  else if (/\bDEBUG\b/i.test(raw)) lv = 'debug';
  return `<div class="logline ${fresh ? 'fresh' : ''}">
    <span class="ts">${ts}</span><span class="lv ${lv}">${lv.toUpperCase()}</span>
    <span class="msg">${esc(raw)}</span></div>`;
}

function renderLogs() {
  const full = $('#fullLog');
  const nearBottom = full.scrollHeight - full.scrollTop - full.clientHeight < 60;
  full.innerHTML = state.logs.length
    ? state.logs.map((l) => logLine(l, false)).join('')
    : '<div class="dim">暂无日志</div>';
  if (nearBottom) full.scrollTop = full.scrollHeight;
  $('#dashLog').innerHTML = state.logs.slice(-7).map((l) => logLine(l, false)).join('')
    || '<div class="dim">暂无日志</div>';
}

/* ------------------------------------------------------------ render: connections */

const connRowMap = new Map(); // 连接 id -> 行元素与可变单元格引用（增量渲染用）

// 行身份指纹：host/链路/规则等静态字段（Clash 会在连接中途补出 host，变了就重建行）
function connIdKey(c) {
  const m = c.metadata || {};
  return [m.host, m.destinationIP, m.destinationPort,
    (c.chains || []).slice(0, 3).join('>'), (c.chains || []).slice(-1)[0],
    c.rule, m.network].join('|');
}

function buildConnRow(c) {
  const m = c.metadata || {};
  const chain = (c.chains || []).slice(0, 3).join(' → ') || '—';
  const out = (c.chains || []).slice(-1)[0] || '—';
  const target = m.destinationIP ? `${m.destinationIP}:${m.destinationPort || ''}` : (m.host || '—');
  const tr = document.createElement('tr');
  tr.innerHTML = `
      <td><div class="cell-main"><span class="conn-ic ${out === 'direct' ? 'direct' : 'proxy'}">
        ${out === 'direct'
          ? '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><path d="M4 12h16M14 6l6 6-6 6"/></svg>'
          : '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><path d="M7 17 17 7M9 7h8v8"/></svg>'}
      </span>
      <div class="col"><div class="t">${esc(m.host || m.destinationIP || '—')}</div><div class="s">${esc((m.network || '').toUpperCase())}</div></div></div></td>
      <td><span class="dim">${esc(target)}</span></td>
      <td><div class="chain-cell">${esc(stripFlag(chain))}</div><div class="rule-cell">${esc(c.rule || 'final')}</div></td>
      <td><span class="badge ${out === 'direct' ? '' : 'lime'}">${esc(m.network || 'tcp')}</span></td>
      <td class="t-right"><span class="conn-rate right-auto"><span class="up">↑</span><span class="rv-up"></span></span></td>
      <td class="t-right"><span class="conn-rate right-auto"><span class="dn">↓</span><span class="rv-dn"></span></span></td>
      <td><span class="dim num"><span class="rv-dur"></span></span></td>`;
  return {
    tr, key: connIdKey(c),
    up: tr.querySelector('.rv-up'),
    dn: tr.querySelector('.rv-dn'),
    dur: tr.querySelector('.rv-dur'),
  };
}

function renderConns() {
  const q = ($('#connSearch')?.value || '').toLowerCase();
  const rows = state.conns.filter((c) => {
    if (!q) return true;
    const m = c.metadata || {};
    return (m.host || '').includes(q) || (c.rule || '').includes(q) || (m.destinationIP || '').includes(q);
  }).slice(0, 200);

  const tbody = $('#connTable tbody');
  if (!rows.length) {
    tbody.innerHTML = `<tr><td colspan="7" class="empty-cell">暂无连接</td></tr>`;
    connRowMap.clear();
  } else {
    if (!connRowMap.size) tbody.innerHTML = ''; // 清掉空态行
    const liveIds = new Set(state.conns.map((c) => c.id));
    const visibleIds = new Set();
    for (const c of rows) {
      let e = connRowMap.get(c.id);
      if (!e) {
        e = buildConnRow(c);
        connRowMap.set(c.id, e);
      } else if (e.key !== connIdKey(c)) {
        // host 后到等身份字段变化：整行重建，原位替换
        const fresh = buildConnRow(c);
        e.tr.replaceWith(fresh.tr);
        Object.assign(e, fresh);
      }
      // 每帧只有三个格子会变：上行 / 下行 / 时长
      e.up.textContent = fmtBytes(c.upload || 0);
      e.dn.textContent = fmtBytes(c.download || 0);
      const dur = c.start ? Math.floor((Date.now() - new Date(c.start).getTime()) / 1000) : 0;
      e.dur.textContent = dur > 3600 ? `${Math.floor(dur / 3600)}h ${Math.floor((dur % 3600) / 60)}m`
        : dur > 60 ? `${Math.floor(dur / 60)}m ${dur % 60}s` : `${Math.max(0, dur)}s`;
      visibleIds.add(c.id);
      tbody.appendChild(e.tr); // 已存在则移动 → 保持过滤后的顺序
    }
    for (const [id, e] of connRowMap) {
      if (!liveIds.has(id)) {
        e.tr.remove();
        connRowMap.delete(id);
      } else if (!visibleIds.has(id)) {
        e.tr.style.display = 'none'; // 被搜索过滤掉：保留行，只隐藏
      } else if (e.tr.style.display === 'none') {
        e.tr.style.display = '';
      }
    }
  }

  $('#connCount').textContent = `${rows.length} / 200`;
  $('#statConns').textContent = state.conns.length;
}

/* ------------------------------------------------------------ traffic chart */

const CHART = { w: 760, h: 208, pl: 34, pr: 8, pt: 14, pb: 24 };

function smoothPath(pts) {
  if (!pts.length) return '';
  let d = `M${pts[0][0]},${pts[0][1]}`;
  for (let i = 1; i < pts.length; i++) {
    const [x0, y0] = pts[i - 1], [x1, y1] = pts[i];
    const cx = (x0 + x1) / 2;
    d += ` C${cx},${y0} ${cx},${y1} ${x1},${y1}`;
  }
  return d;
}

function drawChart() {
  const svg = $('#trafficChart');
  const down = state.traffic.downHist;
  const up = state.traffic.upHist;
  const n = Math.max(down.length, 2);
  const { w, h, pl, pr, pt, pb } = CHART;
  const maxV = Math.max(1024, ...down, ...up) * 1.75;
  const iw = w - pl - pr, ih = h - pt - pb;
  const X = (i) => pl + (i / Math.max(1, n - 1)) * iw;
  const Y = (v) => pt + ih - (v / maxV) * ih;

  const dPts = down.map((v, i) => [X(i), Y(v)]);
  const uPts = up.map((v, i) => [X(i), Y(v)]);

  const C = chartColors();
  const grid = [0, 0.25, 0.5, 0.75, 1].map((f) => {
    const y = pt + ih * f;
    const val = Math.round(maxV * (1 - f));
    return `<line x1="${pl}" y1="${y}" x2="${w - pr}" y2="${y}" stroke="${C.grid}" stroke-width="1" stroke-dasharray="${f === 1 ? '0' : '2 4'}"/>
      <text x="${pl - 8}" y="${y + 3}" fill="${C.axis}" font-size="9.5" font-family="JetBrains Mono, monospace" text-anchor="end">${fmtBytes(val)}</text>`;
  }).join('');

  const xLabels = ['-90s', '-60s', '-30s', '现在'].map((l, i) => {
    const x = pl + (i / 3) * iw;
    return `<text x="${x}" y="${h - 6}" fill="${C.axis}" font-size="9.5" font-family="JetBrains Mono, monospace" text-anchor="${i === 0 ? 'start' : i === 3 ? 'end' : 'middle'}">${l}</text>`;
  }).join('');

  const lastX = X(down.length - 1), lastY = Y(down[down.length - 1] || 0);

  svg.innerHTML = `
    <defs>
      <linearGradient id="gDown" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="${C.down}" stop-opacity="0.26"/><stop offset="100%" stop-color="${C.down}" stop-opacity="0"/></linearGradient>
      <linearGradient id="gUp" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="${C.up}" stop-opacity="0.2"/><stop offset="100%" stop-color="${C.up}" stop-opacity="0"/></linearGradient>
    </defs>
    ${grid}
    ${down.length ? `<path d="${smoothPath(dPts)} L${lastX},${pt + ih} L${pl},${pt + ih} Z" fill="url(#gDown)"/>
    <path d="${smoothPath(dPts)}" fill="none" stroke="${C.down}" stroke-width="1.6" vector-effect="non-scaling-stroke"/>` : ''}
    ${up.length ? `<path d="${smoothPath(uPts)} L${lastX},${pt + ih} L${pl},${pt + ih} Z" fill="url(#gUp)"/>
    <path d="${smoothPath(uPts)}" fill="none" stroke="${C.up}" stroke-width="1.4" stroke-opacity=".9" vector-effect="non-scaling-stroke"/>` : ''}
    ${down.length ? `<circle cx="${lastX}" cy="${lastY}" r="2.6" fill="${C.down}"/>` : ''}
    ${down.length ? '' : `<text x="${(pl + w - pr) / 2}" y="${pt + ih / 2}" fill="${C.axis}" font-size="12" text-anchor="middle" font-family="Inter, sans-serif">等待流量数据…</text>`}
    ${xLabels}`;
}

function cssVar(name, fallback) {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

function chartColors() {
  return {
    down: cssVar('--chart-down', '#c6ff4d'),
    up: cssVar('--chart-up', '#8f7dff'),
    grid: cssVar('--chart-grid', '#191d23'),
    axis: cssVar('--chart-axis', '#6b7280'),
    teal: cssVar('--teal', '#52d9c4'),
    violet: cssVar('--violet', '#8f7dff')
  };
}

function drawSpark(svg, data, color) {
  if (!data || data.length < 2) { svg.innerHTML = ''; return; }
  const w = 90, h = 42, pad = 4;
  const max = Math.max(...data), min = Math.min(...data);
  const pts = data.map((v, i) => [
    (i / (data.length - 1)) * w,
    h - pad - ((v - min) / (max - min || 1)) * (h - pad * 2)
  ]);
  let d = `M${pts[0][0]},${pts[0][1]}`;
  for (let i = 1; i < pts.length; i++) {
    const [x0, y0] = pts[i - 1], [x1, y1] = pts[i];
    const cx = (x0 + x1) / 2;
    d += ` C${cx},${y0} ${cx},${y1} ${x1},${y1}`;
  }
  const gid = 'sg' + Math.random().toString(36).slice(2, 8);
  svg.innerHTML = `
    <defs><linearGradient id="${gid}" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%" stop-color="${color}" stop-opacity="0.22"/>
      <stop offset="100%" stop-color="${color}" stop-opacity="0"/>
    </linearGradient></defs>
    <path d="${d} L${w},${h} L0,${h} Z" fill="url(#${gid})"/>
    <path d="${d}" fill="none" stroke="${color}" stroke-width="1.4" stroke-opacity="0.8"/>`;
}

// 统计卡右下角的趋势条：只有「下行速率」有真实序列，其余留空
function renderSparklines() {
  const C = chartColors();
  const svg = document.querySelector('[data-spark="b"]');
  if (!svg) return;
  const hist = state.traffic.downHist.slice(-24);
  if (hist.length < 3) { svg.innerHTML = ''; return; }
  drawSpark(svg, hist, C.down);
}

function renderTrafficStats() {
  const t = state.traffic;
  const mb = t.down / 1048576;
  $('#statDown').innerHTML = mb >= 1
    ? `${mb.toFixed(1)}<small>MB/s</small>`
    : `${(t.down / 1024).toFixed(t.down > 1024 ? 0 : 1)}<small>KB/s</small>`;
  $('#statUpRate').textContent = `上行 ${fmtRate(t.up)}`;
  $('#connUp').textContent = `↑ ${fmtRate(t.up)}`;
  $('#connDown').textContent = `↓ ${fmtRate(t.down)}`;
}

/* ------------------------------------------------------------ websockets */

let trafficWS = null, connWS = null;

// 免登录模式下后端不校验 token，浏览器也没有 token 可带；
// 只有「需要登录且尚未登录」时才应该放弃建立 WebSocket。
function wsAllowed() {
  return !state.authRequired || !!state.token;
}

function wsURL(path) {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const q = state.token ? `?token=${encodeURIComponent(state.token)}` : '';
  return `${proto}://${location.host}${path}${q}`;
}

function connectTraffic() {
  if (trafficWS) trafficWS.close();
  let retry = 1000;
  const open = () => {
    if (!wsAllowed()) return;
    const ws = new WebSocket(wsURL('/api/v1/ws/traffic'));
    trafficWS = ws;
    ws.onmessage = (e) => {
      try {
        const d = JSON.parse(e.data);
        state.traffic.up = d.up || 0;
        state.traffic.down = d.down || 0;
        state.traffic.upHist.push(state.traffic.up);
        state.traffic.downHist.push(state.traffic.down);
        if (state.traffic.upHist.length > 48) state.traffic.upHist.shift();
        if (state.traffic.downHist.length > 48) state.traffic.downHist.shift();
        // 后台标签页：数据照收（图表/统计在回前台时一次性补齐）
        if (!document.hidden) {
          drawChart();
          renderTrafficStats();
          renderSparklines();
        }
      } catch (_) {}
    };
    ws.onopen = () => { retry = 1000; };
    ws.onclose = () => {
      if (!wsAllowed()) return;
      setTimeout(open, retry);
      retry = Math.min(retry * 2, 30000);
    };
  };
  open();
}

let connLastFrame = 0;

// WS 正常时不轮询；超过 4 秒没有帧就走 REST 兜底，
// 避免个别网络环境下 WebSocket 静默失效导致连接页永远是空的。
function startConnFallback() {
  if (connFallbackTimer) clearInterval(connFallbackTimer);
  connFallbackTimer = setInterval(async () => {
    if (!wsAllowed() || state.paused) return;
    if (Date.now() - connLastFrame < 4000) return;
    try {
      const d = await api('/api/v1/clash/connections');
      state.conns = (d.connections || []).slice(0, 200);
      if (!state.paused && !document.hidden) renderConns();
    } catch (_) {}
  }, 2000);
}

function connectConnections() {
  if (connWS) connWS.close();
  startConnFallback();
  let retry = 1000;
  const open = () => {
    if (!wsAllowed()) return;
    const ws = new WebSocket(wsURL('/api/v1/ws/connections'));
    connWS = ws;
    ws.onopen = () => {
      retry = 1000;
      $('#connDot').style.display = '';
      $('#connLive').className = 'chip ok';
      $('#connLive').innerHTML = '<i class="dot live"></i> 实时';
    };
    ws.onmessage = (e) => {
      try {
        const d = JSON.parse(e.data);
        connLastFrame = Date.now();
        if (!state.paused && Array.isArray(d.connections)) {
          state.conns = d.connections.slice(0, 200);
          if (!document.hidden) renderConns();
        }
      } catch (_) {}
    };
    ws.onclose = () => {
      $('#connDot').style.display = 'none';
      $('#connLive').className = 'chip warn';
      $('#connLive').innerHTML = '<i class="dot"></i> 重连中';
      if (!wsAllowed()) return;
      setTimeout(open, retry);
      retry = Math.min(retry * 2, 30000);
    };
  };
  open();
}

// 后台标签页暂停了重渲染（数据照收），回到前台一次性补齐
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState !== 'visible' || !state.status) return;
  renderStatus();
  renderTrafficStats();
  drawChart();
  renderSparklines();
  renderConns();
  if (logsVisible()) loadLogs();
});

/* ------------------------------------------------------------ actions */

async function serviceAction(action) {
  const labels = { start: '启动', stop: '停止', restart: '重启' };
  const ok = await confirmDialog({
    title: `${labels[action]} ${eng()} 核心？`,
    body: action === 'stop'
      ? '停止后所有通过面板的代理流量会立即中断。'
      : '现有连接会短暂中断，通常 1 秒内恢复。',
    confirm: labels[action],
    danger: action === 'stop'
  });
  if (!ok) return;
  try {
    await api(`/api/v1/service/${action}`, { method: 'POST' });
    toast('ok', `已${labels[action]}`, action === 'stop' ? '核心已停止' : '核心已启动');
    await pollStatus();
  } catch (e) {
    toast('err', `${labels[action]}失败`, e.message);
  }
}

$$('[data-action]').forEach((b) => (b.onclick = () => serviceAction(b.dataset.action)));
$('#refreshBtn').onclick = async () => {
  await Promise.all([pollStatus(), loadLogs()]);
  toast('ok', '已刷新', '状态与日志已重新拉取');
};
$('#testAllBtn').onclick = testAll;
$('#refreshAllBtn').onclick = async () => {
  for (const s of state.subs) await refreshSub(s.id);
};
$('#addSubBtn').onclick = openAddSub;
$('#renderBtn').onclick = renderFromTemplate;
$('#validateBtn').onclick = validateConfig;
$('#applyBtn').onclick = applyConfig;
$('#rollbackBtn').onclick = () => {
  const target = state.snapshots[0];
  if (!target) { toast('info', '没有可回滚的快照'); return; }
  rollbackTo(target.version);
};
$('#nodeSearch').oninput = (e) => { nodeFilter.q = e.target.value; renderNodes(); };
$('#connSearch').oninput = () => renderConns();
$('#pauseBtn').onclick = (e) => {
  state.paused = !state.paused;
  e.target.textContent = state.paused ? '继续' : '暂停';
  if (!state.paused) renderConns();
};
$('#copyLogBtn').onclick = () => {
  navigator.clipboard?.writeText(state.logs.map((l) => l.raw).join('\n'));
  toast('ok', '已复制', `${state.logs.length} 行日志`);
};
async function loadTproxy() {
  if (!isLinux()) return;
  try {
    const d = await api('/api/v1/tproxy');
    state.tproxy = d;
    $('#tproxySwitch').classList.toggle('on', !!d.enabled);
    $('#tproxyBadge').textContent = d.enabled ? '已接管' : '未启用';
    $('#tproxyBadge').className = 'badge ' + (d.enabled ? 'lime' : 'amber');
    $('#tpRules').textContent = `${d.rules} 条`;
    $('#tpMode').textContent = d.mode;
    $('#tpCidrs').textContent = (d.cidrs && d.cidrs.length) ? d.cidrs.join('  ') : '未检测到';
    $('#tpCore').textContent = state.status?.running ? '运行中' : '未运行';
    $('#tpPreviewNote').textContent = d.enabled ? '当前生效' : '启用后下发';
    // 明细（规则数/网段/本机/DNS）只在启用后展示，未启用时保持行面干净
    if ($('#tpRedirectGrid')) $('#tpRedirectGrid').style.display = d.enabled ? '' : 'none';
    $('#localSwitch').classList.toggle('on', !!d.local);
    $('#dnsSwitch').classList.toggle('on', !!d.dns);
    $('#localSwitch').disabled = !d.local_supported;
    $('#topStatus').dataset.tproxy = d.enabled ? '1' : '0';
  } catch (e) { /* ignore */ }
}

$('#tproxyRefresh').onclick = loadTproxy;

async function loadLanProxy() {
  try {
    const d = await api('/api/v1/lanproxy');
    state.lanproxy = d;
    $('#lpSwitch').classList.toggle('on', !!d.enabled);
    $('#lpSocks').value = d.socks_port || 1080;
    $('#lpHttp').value = d.http_port || 8118;
    // 直接回填已保存的值（可能为空）；不再隐式填充默认用户名，
    // 这样把两个框都清空就代表「不鉴权（开放代理）」。
    $('#lpUser').value = d.username || '';
    $('#lpPass').value = d.password || '';
    $('#lpBadge').textContent = d.enabled ? '已开启' : '未开启';
    $('#lpBadge').className = 'badge ' + (d.enabled ? 'lime' : '');
    const open = d.enabled && !d.username && !d.password;
    $('#lpOpenWarn').style.display = open ? '' : 'none';
    if (open) {
      $('#lpOpenPorts').textContent = d.socks_port + ' / ' + d.http_port;
    }
    if (d.enabled && d.socks_url) {
      $('#lpHint').style.display = '';
      $('#lpLinks').innerHTML = `${esc(d.socks_url)}<br>${esc(d.http_url)}`;
    } else {
      $('#lpHint').style.display = 'none';
    }
    renderLanProxyStats(d);
    renderMixedInbound(d);
  } catch (e) { /* ignore */ }
}

// renderMixedInbound 回填 mixed 入站（系统代理入口）的端口与监听范围。
// 它与局域网代理开关无关：即使局域网代理关着，mixed 也一直存在。
function renderMixedInbound(d) {
  const port = d.mixed_port || 7890;
  const listen = d.mixed_listen || '127.0.0.1';
  $('#lpMixPort').value = port;
  $('#lpMixBadge').textContent = `${listen}:${port}`;
  $$('#lpMixListenSeg button').forEach((b) => b.classList.toggle('on', b.dataset.listen === listen));
  $('#lpMixOpenWarn').style.display = listen === '0.0.0.0' || listen === '::' ? '' : 'none';
}

// renderLanProxyStats 把接口返回值铺到「当前状态」卡片上。
// 端口监听由 sing-box 的入站决定：核心没跑起来时开关为开也不会真的监听。
function renderLanProxyStats(d) {
  const host = location.hostname || '127.0.0.1';
  const running = !!state.status?.running;
  const live = !!d.enabled && running;
  $('#lpChip').innerHTML = `<i class="dot${live ? ' live' : ''}"></i> ${live ? '运行中' : d.enabled ? '待核心启动' : '未开启'}`;
  $('#lpStatRun').textContent = live ? '监听中' : d.enabled ? '已开启 · 核心未运行' : '未开启';
  $('#lpStatRun').className = 'badge ' + (live ? 'lime' : d.enabled ? 'amber' : '');
  $('#lpStatSocks').textContent = d.enabled ? `${host}:${d.socks_port}` : '—';
  $('#lpStatHttp').textContent = d.enabled ? `${host}:${d.http_port}` : '—';
  const hasCred = !!d.username && !!d.password;
  $('#lpStatAuth').textContent = !d.enabled ? '—' : hasCred ? `用户名密码（${d.username}）` : '无鉴权';
  $('#lpStatAuth').className = 'badge ' + (!d.enabled ? '' : hasCred ? 'lime' : 'amber');
}

// lanProxyBody 收集这一页上的全部字段。
// 接口是「整体保存」，所以改 mixed 端口时也必须把局域网代理的当前值一起带上，
// 否则会把另一块设置冲掉。
function lanProxyBody() {
  return {
    enabled: $('#lpSwitch').classList.contains('on'),
    socks_port: parseInt($('#lpSocks').value, 10) || 0,
    http_port: parseInt($('#lpHttp').value, 10) || 0,
    username: $('#lpUser').value.trim(),
    password: $('#lpPass').value,
    mixed_port: parseInt($('#lpMixPort').value, 10) || 0,
    mixed_listen: $('#lpMixListenSeg button.on')?.dataset.listen || '127.0.0.1'
  };
}

async function saveLanProxy(kind) {
  const body = lanProxyBody();
  const isMixed = kind === 'mixed';
  const open = body.enabled && !body.username && !body.password;
  let confirmBody = '保存后重启核心，连接短暂中断。';
  let title = '保存局域网代理设置？';
  if (isMixed) {
    title = '保存 mixed 入站设置？';
    confirmBody = `mixed 入站将改为 <b>${body.mixed_listen}:${body.mixed_port}</b>，重启核心生效。`;
  } else if (open) {
    confirmBody = '将创建无鉴权开放代理：局域网内任意设备都可免密使用。';
  }
  const ok = await confirmDialog({ title, body: confirmBody, confirm: '保存并应用' });
  if (!ok) return;
  const btn = isMixed ? $('#lpMixSave') : $('#lpSave');
  btn.disabled = true;
  try {
    const d = await api('/api/v1/lanproxy', { method: 'PUT', body: JSON.stringify(body) });
    let msg;
    if (isMixed) {
      msg = `mixed 入站已改为 ${d.mixed_url}`;
    } else {
      msg = d.enabled
        ? (d.open ? '已开启（无鉴权开放代理）' : `SOCKS5 ${d.socks_url.split('@').pop()} · HTTP ${d.http_url.split('@').pop()}`)
        : '局域网代理已关闭';
    }
    toast('ok', '已应用', msg);
    await loadLanProxy();
    await pollStatus();
  } catch (e) {
    toast('err', '保存失败', e.message);
  } finally {
    btn.disabled = false;
  }
}

$('#lpGen').onclick = () => {
  const bytes = crypto.getRandomValues(new Uint8Array(8));
  $('#lpPass').value = Array.from(bytes).map((b) => b.toString(16).padStart(2, '0')).join('');
};

$('#lpSave').onclick = () => saveLanProxy('lan');
$('#lpMixSave').onclick = () => saveLanProxy('mixed');
$('#lpRefresh').onclick = loadLanProxy;

// 监听范围切换只改本地选中态，保存时才下发
$$('#lpMixListenSeg button').forEach((b) => {
  b.onclick = () => {
    $$('#lpMixListenSeg button').forEach((x) => x.classList.toggle('on', x === b));
    const listen = b.dataset.listen;
    $('#lpMixOpenWarn').style.display = listen === '0.0.0.0' ? '' : 'none';
    $('#lpMixBadge').textContent = `${listen}:${$('#lpMixPort').value || 7890}`;
  };
});
$('#lpMixPort').oninput = () => {
  const listen = $('#lpMixListenSeg button.on')?.dataset.listen || '127.0.0.1';
  $('#lpMixBadge').textContent = `${listen}:${$('#lpMixPort').value || 7890}`;
};

$('#dnsSwitch').onclick = async () => {
  const next = !(state.tproxy && state.tproxy.dns);
  if (!state.tproxy?.enabled) {
    toast('info', '先开启透明代理', 'DNS 接管依赖 redirect 规则');
    return;
  }
  if (next) {
    const ok = await confirmDialog({
      title: '接管局域网 DNS？',
      body: '53 端口查询将劫持到当前内核：<code>' + ((state.tproxy?.cidrs || []).join('  ')) + '</code>',
      confirm: '开始接管'
    });
    if (!ok) return;
  }
  try {
    const d = await api('/api/v1/tproxy', { method: 'PUT', body: JSON.stringify({ enabled: true, dns: next }) });
    toast(next ? 'ok' : 'info', next ? '已接管局域网 DNS' : '已恢复局域网 DNS',
      next ? `${d.rules} 条规则` : '53 端口不再劫持');
    await loadTproxy();
  } catch (e) {
    toast('err', '操作失败', e.message);
    await loadTproxy();
  }
};

$('#localSwitch').onclick = async () => {
  const next = !(state.tproxy && state.tproxy.local);
  if (!state.tproxy?.enabled) {
    toast('info', '先开启透明代理', '本机接管依赖 redirect 规则');
    return;
  }
  try {
    const d = await api('/api/v1/tproxy', {
      method: 'PUT',
      body: JSON.stringify({ enabled: true, local: next })
    });
    toast(next ? 'ok' : 'info', next ? '已接管本机流量' : '已恢复本机直连',
      next ? `${d.rules} 条规则` : 'output 链已移除');
    await loadTproxy();
  } catch (e) {
    toast('err', '操作失败', e.message);
    await loadTproxy();
  }
};

$('#tproxySwitch').onclick = async () => {
  const next = !(state.tproxy && state.tproxy.enabled);
  if (next) {
    const ok = await confirmDialog({
      title: '开启透明代理？',
      body: '接管以下网段的 TCP 流量：<code>' + ((state.tproxy?.cidrs || []).join('  ') || '自动检测') + '</code>',
      confirm: '开启 redirect'
    });
    if (!ok) return;
  }
  try {
    const d = await api('/api/v1/tproxy', { method: 'PUT', body: JSON.stringify({ enabled: next }) });
    toast(next ? 'ok' : 'info', next ? '透明代理已开启' : '透明代理已关闭',
      next ? `${d.rules} 条规则已下发` : 'nftables 表已删除，流量恢复直连');
    if (d.warning) toast('info', '注意', d.warning);
    await loadTproxy();
  } catch (e) {
    toast('err', '操作失败', e.message);
    await loadTproxy();
  }
};

/* ------------------------------------------------------------ tun */

async function loadTun() {
  try {
    const d = await api('/api/v1/tun');
    state.tun = d;
    const on = !!d.running;
    if ($('#tunDevName')) $('#tunDevName').textContent = engTunDev();
    const linux = isLinux();
    $('#tunSwitch2').classList.toggle('on', on);
    $('#tunBadge').textContent = on ? '已接管' : '未启用';
    $('#tunBadge').className = 'badge ' + (on ? 'lime' : 'amber');
    $('#tunAdmin').textContent = d.supported === false
      ? (linux ? '面板未以 root 运行' : '面板未以管理员运行')
      : (linux ? 'root 身份运行' : '管理员身份运行');
    $('#tunAdmin').className = 'badge ' + (d.supported === false ? 'red' : 'lime');
    // 权限行只在启用后展示
    if ($('#tpTunGrid')) $('#tpTunGrid').style.display = on ? '' : 'none';
    updateEntryStatus();
  } catch (e) { /* ignore */ }
}

// —— 系统代理（Windows）——
async function loadSysproxy() {
  if (isLinux()) return;
  try {
    const d = await api('/api/v1/sysproxy');
    state.sysproxy = d;
    const sw = $('#sysproxySwitch');
    if (sw) { sw.classList.toggle('on', !!d.enabled); sw.disabled = false; }
    const b = $('#sysproxyBadge');
    if (b) { b.textContent = d.enabled ? '已接管' : '未启用'; b.className = 'badge ' + (d.enabled ? 'lime' : 'amber'); }
    const srv = $('#sysproxyServer');
    if (srv) srv.textContent = d.server || '—';
  } catch (e) { /* ignore */ }
  updateEntryStatus();
}

$('#sysproxySwitch').onclick = async () => {
  const sw = $('#sysproxySwitch');
  const next = !sw.classList.contains('on');
  sw.disabled = true;
  const badge = $('#sysproxyBadge');
  badge.textContent = '设置中…';
  badge.className = 'badge amber';
  try {
    await api('/api/v1/sysproxy', { method: 'PUT', body: JSON.stringify({ enabled: next }) });
    toast(next ? 'ok' : 'info', next ? '系统代理已开启' : '系统代理已关闭',
      next ? '浏览器即时生效，无需重启' : '已还原开启前状态');
  } catch (e) {
    toast('err', '设置失败', e.message);
  }
  await loadSysproxy();
};

// 入口（系统代理/TUN）与出口（路由模式）的关系提示条
function updateEntryStatus() {
  const el = $('#entryStatus');
  if (!el || isLinux()) { if (el) el.style.display = 'none'; return; }
  const sys = !!(state.sysproxy && state.sysproxy.enabled);
  const tun = !!(state.tun && state.tun.running);
  let text, warn = false;
  if (!sys && !tun) {
    text = '没有任何入口开启，路由模式对普通流量不生效——开启「系统代理」或「TUN 虚拟网卡」其一';
    warn = true;
  } else if (sys && tun) {
    text = 'TUN 已接管全部流量，系统代理冗余，建议关闭其一';
  } else if (tun) {
    text = 'TUN 已接管整机流量（含不遵守系统代理的程序）';
  } else {
    text = '遵守系统代理的程序已接管；游戏等直连程序不受影响';
  }
  const t = $('#entryStatusText');
  if (t) t.textContent = text;
  el.classList.toggle('warn', warn);
  el.style.display = '';
}

async function tunToggle() {
  const on = !!state.tun?.running;
  const next = !on;
  const who = isLinux() ? 'root' : '管理员';
  const ok = await confirmDialog({
    title: next ? '开启 TUN 模式？' : '关闭 TUN 模式？',
    body: next
      ? `创建 <code>${engTunDev()}</code> 虚拟网卡接管整机流量，需以${who}运行；连接中断约 1 秒。`
      : '移除 TUN 入站并重启核心，整机流量恢复直连。',
    confirm: next ? '开启 TUN' : '关闭 TUN'
  });
  if (!ok) return;
  try {
    await api('/api/v1/tun', { method: 'PUT', body: JSON.stringify({ enabled: next }) });
    toast(next ? 'ok' : 'info', next ? 'TUN 已开启' : 'TUN 已关闭',
      next ? `虚拟网卡已创建，整机流量现在经由 ${engLabel()}` : '虚拟网卡已移除，流量恢复直连');
  } catch (e) {
    toast('err', '操作失败', e.message);
  }
  // 核心重启后 Clash API/配置落定需要一点时间，稍等再取真实状态
  setTimeout(loadTun, 1500);
}

$('#tunSwitch2').onclick = tunToggle;

/* ------------------------------------------------------------ exit ip */

// 首页「运行中」旁边的出口 IP 徽标：后端经内核 mixed 入站实测，60s 缓存
async function loadExitIp() {
  const el = $('#heroExitIp');
  if (!el) return;
  try {
    const running = !!(state.status && state.status.running);
    if (!running) {
      el.style.display = 'none';
      return;
    }
    const d = await api('/api/v1/exitip');
    if (d && d.ip) {
      el.textContent = `出口 ${d.ip}`;
      el.title = d.via === 'proxy' ? '当前出口 IP · 经内核入站实测' : '当前出口 IP · 直连测得';
      el.style.display = '';
    } else {
      el.style.display = 'none';
    }
  } catch (e) { /* ignore */ }
}

/* ------------------------------------------------------------ access control */

$('#authSwitch').onclick = async () => {
  const s = state.status;
  if (!s) return;
  try {
    if (s.login_required) {
      const ok = await confirmDialog({
        title: '关闭登录？',
        body: `任何能访问 <code>${esc(s.listen)}</code> 的人都可以直接操作面板。`,
        confirm: '关闭登录',
        danger: true
      });
      if (!ok) return;
      await api('/api/v1/auth/access', { method: 'PUT', body: JSON.stringify({ disabled: true }) });
      toast('ok', '登录已关闭', '现在打开面板无需账号密码');
    } else {
      const cred = await authCredDialog(s.username);
      if (!cred) return;
      await api('/api/v1/auth/access', {
        method: 'PUT',
        body: JSON.stringify({ disabled: false, username: cred.username, password: cred.password })
      });
      await login(cred.username, cred.password);
      toast('ok', '登录已启用', '其他已登录会话已失效');
    }
    state.status = await api('/api/v1/status');
    renderStatus();
  } catch (e) {
    toast('err', '设置失败', e.message || String(e));
  }
};

/* ------------------------------------------------------------ listen address */

async function saveListen() {
  const addr = $('#setListenInput').value.trim();
  if (!addr) { toast('err', '请填写监听地址'); return; }
  const btn = $('#setListenSave');
  btn.disabled = true;
  try {
    const res = await api('/api/v1/settings/listen', {
      method: 'PUT',
      body: JSON.stringify({ listen: addr })
    });
    toast('ok', '监听地址已更新', res.listen || addr);
    if (res.warning) toast('info', '注意', res.warning);
    try {
      state.status = await api('/api/v1/status');
      renderStatus();
    } catch (e) {
      toast('info', '请使用新地址访问', `监听已切换为 ${addr}，当前页面可能无法继续连接`);
    }
  } catch (e) {
    toast('err', '修改失败', e.message || String(e));
    if (state.status) $('#setListenInput').value = state.status.listen;
  } finally {
    btn.disabled = false;
  }
}

$('#setListenSave').onclick = saveListen;
$('#setListenInput').onkeydown = (e) => {
  if (e.key === 'Enter') saveListen();
};

/* ------------------------------------------------------------ routing */

const ROUTE_META = {
  rule: { name: '规则分流', badge: 'badge lime' },
  global: { name: '全局代理', badge: 'badge amber' },
  direct: { name: '全局直连', badge: 'badge' }
};

function renderRouting(mode) {
  const meta = ROUTE_META[mode] || ROUTE_META.rule;
  $$('#routeSeg button').forEach((b) => b.classList.toggle('on', b.dataset.mode === mode));
  $('#routeBadge').textContent = meta.name;
  $('#routeBadge').className = meta.badge;
}

async function loadRouting() {
  try {
    const d = await api('/api/v1/routing');
    state.routing = d;
    renderRouting(d.mode);
  } catch (e) { /* ignore */ }
}

async function setRouting(mode) {
  const meta = ROUTE_META[mode];
  if (!meta || (state.routing && state.routing.mode === mode)) return;
  const ok = await confirmDialog({
    title: `切换到「${meta.name}」？`,
    body: '重新渲染配置并重启核心，连接短暂中断；失败自动回滚。',
    confirm: '切换模式'
  });
  if (!ok) return;
  const buttons = $$('#routeSeg button');
  buttons.forEach((b) => (b.disabled = true));
  try {
    await api('/api/v1/routing', { method: 'PUT', body: JSON.stringify({ mode }) });
    toast('ok', '路由模式已切换', meta.name);
    await loadRouting();
    await pollStatus();
  } catch (e) {
    toast('err', '切换失败', e.message);
    await loadRouting();
  } finally {
    buttons.forEach((b) => (b.disabled = false));
  }
}

$$('#routeSeg button').forEach((b) => (b.onclick = () => setRouting(b.dataset.mode)));

/* ------------------------------------------------------------ engine */

async function loadEngine() {
  try {
    state.engine = await api('/api/v1/engine');
  } catch (e) {
    state.engine = null;
  }
  try {
    state.geo = (await api('/api/v1/geodata')).geo;
  } catch (e) {
    state.geo = null;
  }
  renderEngine();
}

// geo 数据是 mihomo 启动的前提（缺了它会自己下载，实测 31 秒）。
// 这里给一行状态，让人知道切过去要不要等。
function geoLabel(g) {
  if (!g) return '';
  switch (g.state) {
    case 'ready': return `已就绪${g.embedded ? '（内置）' : ''}`;
    case 'downloading': {
      // jsDelivr 走 chunked 传输时拿不到 Content-Length，只能退化成显示已下载量
      if (g.total_bytes > 0) {
        return `下载中 ${Math.round((g.bytes / g.total_bytes) * 100)}%`;
      }
      if (g.bytes > 0) return `下载中 ${(g.bytes / 1048576).toFixed(1)} MB`;
      return '下载中…';
    }
    case 'failed': return '下载失败';
    default: return (g.missing || []).length ? '待预取' : '已就绪';
  }
}

function renderEngine() {
  const d = state.engine;
  const note = $('#engineNote');
  if (!note) return;
  if (!d) { note.textContent = '读取内核信息失败'; return; }
  const cur = d.engine;
  $('#engineBadge').textContent = d.label || cur;
  $$('#engineSeg button').forEach((b) => {
    const on = b.dataset.engine === cur;
    b.classList.toggle('on', on);
    const item = (d.available || []).find((x) => x.name === b.dataset.engine);
    // 当前内核保持可点（点了直接返回），只禁用真正不可用的那个
    b.disabled = !item || !item.ok;
    b.title = item && !item.ok ? (item.error || '不可用') : '';
  });
  note.innerHTML = cur === 'mihomo'
    ? `mihomo · Clash YAML · <code>${esc(d.config)}</code>`
    : `sing-box · JSON · <code>${esc(d.config)}</code>`;
  const g = state.geo;
  const geoRow = g ? `
    <div class="kv">
      <span class="k">geo 数据（mihomo 用）</span>
      <span class="badge mono" title="${esc(g.state === 'failed' ? (g.error || '') : (g.missing || []).join('、'))}">${esc(geoLabel(g))}</span>
    </div>` : '';
  $('#engineDetail').innerHTML = (d.available || []).map((it) => `
    <div class="kv">
      <span class="k">${esc(it.label)}</span>
      <span class="badge mono ellipsis" title="${esc(it.version || it.bin || '')}">${it.ok ? esc(it.version_short || it.version || '可用') : '不可用'}</span>
    </div>`).join('') + geoRow;
}

async function switchEngine(name) {
  if (name === eng()) return;
  // 切 mihomo 前先看 geo 数据在不在：缺文件时 mihomo 会自己下载，实测 31 秒。
  // 面板通常已经在后台下好了，这里只把「要不要等」如实告诉用户。
  let geo = null;
  if (name === 'mihomo') {
    try { geo = (await api('/api/v1/geodata')).geo; } catch (e) { /* 老版本没有这个接口 */ }
  }
  const needGeo = !!geo && geo.state !== 'ready';
  const geoNote = !needGeo ? ''
    : `首次启动需先下载 geo 数据（${(geo.missing || []).join('、') || 'GeoSite.dat、geoip.metadb'}）。`;
  const ok = await confirmDialog({
    title: `切换到 ${name}？`,
    body: `按 ${name} 重新渲染配置并重启核心；失败自动切回。` + geoNote,
    confirm: '切换内核'
  });
  if (!ok) return;
  const btn = $$('#engineSeg button').find((b) => b.dataset.engine === name);
  if (btn) { btn.disabled = true; btn.textContent = '切换中…'; }
  // 下载期间把进度打到按钮上，避免十几秒里界面像卡死
  let timer = null;
  if (needGeo) {
    timer = setInterval(async () => {
      try {
        const g = (await api('/api/v1/geodata')).geo;
        if (!g || g.state !== 'downloading') return;
        if (btn) btn.textContent = geoLabel(g).replace('下载中', 'geo');
      } catch (e) { /* 忽略轮询失败 */ }
    }, 800);
  }
  try {
    const r = await api('/api/v1/engine', { method: 'PUT', body: JSON.stringify({ engine: name }) });
    toast('ok', '内核已切换', `${r.label || name} ${r.version || ''}`.trim());
    if (r.warning) toast('info', '注意', r.warning);
  } catch (e) {
    toast('err', '切换失败', e.message);
  } finally {
    if (timer) clearInterval(timer);
  }
  await Promise.all([pollStatus(), loadEngine(), loadConfig()]);
  renderConfig();
  const b2 = $$('#engineSeg button').find((b) => b.dataset.engine === name);
  if (b2) b2.textContent = name;
}

async function loadAutostart() {
  try {
    const d = await api('/api/v1/autostart');
    state.autostart = d;
    $('#asPanelSwitch').classList.toggle('on', !!d.panel);
    $('#asCoreSwitch').classList.toggle('on', d.core !== false);
    $('#asPanelNote').textContent = d.panel_note || '—';
    $('#asPanelSwitch').disabled = d.panel_supported === false;
  } catch (e) { /* ignore */ }
}

$$('#engineSeg button').forEach((b) => {
  b.onclick = () => switchEngine(b.dataset.engine);
});

$('#asCoreSwitch').onclick = async () => {
  const next = !$('#asCoreSwitch').classList.contains('on');
  try {
    await api('/api/v1/autostart', { method: 'PUT', body: JSON.stringify({ core: next }) });
    toast('ok', '已保存', next ? '面板启动时将自动拉起核心' : '面板启动时不再自动拉起核心');
    await loadAutostart();
  } catch (e) {
    toast('err', '操作失败', e.message);
  }
};

$('#asPanelSwitch').onclick = async () => {
  const next = !$('#asPanelSwitch').classList.contains('on');
  const linux = isLinux();
  const ok = await confirmDialog({
    title: next ? '注册面板开机自启？' : '取消面板开机自启？',
    body: linux
      ? `<code>systemctl ${next ? 'enable' : 'disable'} sing-box-panel</code>`
      : `${next ? '注册' : '删除'}计划任务 <code>sing-box-panel</code>（SYSTEM，最高权限）。`,
    confirm: next ? '注册自启' : '取消自启'
  });
  if (!ok) return;
  $('#asPanelSwitch').disabled = true;
  try {
    await api('/api/v1/autostart', { method: 'PUT', body: JSON.stringify({ panel: next }) });
    toast(next ? 'ok' : 'info', next ? '面板开机自启已注册' : '面板开机自启已取消',
      next ? '重启电脑后自动运行' : '重启电脑后不再自动运行');
    await loadAutostart();
  } catch (e) {
    toast('err', '操作失败', e.message);
    await loadAutostart();
  } finally {
    $('#asPanelSwitch').disabled = false;
  }
};

// —— 面板自更新（关于卡）——
async function loadUpdateStatus() {
  const badge = $('#updBadge');
  const btn = $('#updateBtn');
  if (!badge || !btn) return;
  badge.textContent = '检查中…';
  try {
    const d = await api('/api/v1/update');
    if (!d.supported) { badge.textContent = '不支持'; badge.className = 'badge'; btn.style.display = 'none'; return; }
    if (d.has_update) {
      badge.textContent = `可更新到 v${d.latest}`;
      badge.className = 'badge amber';
      btn.textContent = `更新到 v${d.latest}`;
      btn.dataset.ready = d.latest;
    } else {
      badge.textContent = `已是最新 v${d.current}`;
      badge.className = 'badge lime';
      btn.textContent = '检查更新';
      delete btn.dataset.ready;
    }
  } catch (e) {
    badge.textContent = '检查失败';
    badge.className = 'badge red';
  }
  btn.disabled = false;
}

$('#updateBtn').onclick = async () => {
  const btn = $('#updateBtn');
  if (btn.dataset.ready) {
    const ver = btn.dataset.ready;
    const ok = await confirmDialog({
      title: `更新面板到 v${ver}？`,
      body: '自动下载并替换面板二进制，随后面板自动重启（核心会短暂中断，几秒内自动拉起），页面稍后恢复。',
      confirm: '更新并重启'
    });
    if (!ok) return;
    btn.disabled = true;
    badge.textContent = '更新中…';
    badge.className = 'badge amber';
    try {
      await api('/api/v1/update', { method: 'POST' });
      toast('ok', `面板已更新到 v${ver}`, '面板重启中，页面将在几秒后自动恢复…');
      // 轮询等新面板起来，然后整页刷新（拿到新版本号与新前端）
      const poll = setInterval(() => {
        fetch('/api/v1/auth/status').then((r) => {
          if (r.ok) { clearInterval(poll); location.reload(); }
        }).catch(() => {});
      }, 2000);
      setTimeout(() => { clearInterval(poll); location.reload(); }, 60000);
    } catch (e) {
      toast('err', '更新失败', e.message);
      btn.disabled = false;
      loadUpdateStatus();
    }
  } else {
    btn.disabled = true;
    badge.textContent = '检查中…';
    await loadUpdateStatus();
  }
};

/* ------------------------------------------------------------ router */

const PAGE_META = {
  dashboard: '概览', subs: '订阅', nodes: '节点',
  config: '配置', service: '服务', tproxy: '透明代理',
  lanproxy: '局域网代理', conns: '连接', settings: '设置'
};

function go(page) {
  state.page = page;
  $$('.page').forEach((p) => p.classList.toggle('active', p.id === `page-${page}`));
  $$('.nav-item').forEach((n) => n.classList.toggle('active', n.dataset.page === page));
  let t = PAGE_META[page] || '概览';
  if (page === 'tproxy' && !isLinux()) t = '代理模式';
  $('#crumbTitle').textContent = t;
  $('#content').scrollTop = 0;
  if (location.hash !== `#${page}`) history.replaceState(null, '', `#${page}`);
  // 进入局域网代理页时重新拉一次，避免显示别的页面停留期间改过的旧值
  if (page === 'lanproxy') loadLanProxy();
  // 日志在别的页面不轮询，切回来先补一次，不然会显示几分钟前的尾巴
  if (page === 'dashboard' || page === 'service') loadLogs();
}

window.addEventListener('hashchange', () => {
  const p = location.hash.replace('#', '');
  if (PAGE_META[p] && p !== state.page) go(p);
});

$$('.nav-item').forEach((n) => n.onclick = () => go(n.dataset.page));
$$('[data-goto]').forEach((b) => b.onclick = () => go(b.dataset.goto));

/* ------------------------------------------------------------ palette */

const COMMANDS = [
  { g: '页面', t: '概览', p: 'dashboard', i: 'i-dash' },
  { g: '页面', t: '订阅管理', p: 'subs', i: 'i-link' },
  { g: '页面', t: '节点列表', p: 'nodes', i: 'i-server' },
  { g: '页面', t: '配置编辑', p: 'config', i: 'i-code' },
  { g: '页面', t: '服务控制', p: 'service', i: 'i-activity' },
  { g: '页面', t: '代理模式', p: 'tproxy', i: 'i-shield' },
  { g: '页面', t: '局域网代理', p: 'lanproxy', i: 'i-lan' },
  { g: '页面', t: '实时连接', p: 'conns', i: 'i-list' },
  { g: '操作', t: '重启内核', a: () => serviceAction('restart'), i: 'i-refresh' },
  { g: '操作', t: '全部节点测速', a: () => { go('nodes'); testAll(); }, i: 'i-zap' },
  { g: '操作', t: '校验当前配置', a: () => { go('config'); validateConfig(); }, i: 'i-check' },
  { g: '操作', t: '新增订阅', a: () => { go('subs'); openAddSub(); }, i: 'i-plus' }
];

let palIdx = 0;

function renderPalette(q = '') {
  const items = COMMANDS.filter((c) => c.t.toLowerCase().includes(q.toLowerCase()));
  palIdx = 0;
  let html = '', last = '';
  items.forEach((c, i) => {
    if (c.g !== last) { html += `<div class="palette-group">${c.g}</div>`; last = c.g; }
    html += `<button class="palette-item ${i === 0 ? 'on' : ''}" data-i="${i}"><svg width="14" height="14"><use href="#${c.i}"/></svg>${c.t}</button>`;
  });
  $('#paletteList').innerHTML = html || `<div class="empty-cell">无匹配命令</div>`;
  $('#paletteList')._items = items;
  $$('.palette-item').forEach((el) => (el.onclick = () => execPalette(items[+el.dataset.i])));
}

function execPalette(c) {
  $('#paletteOverlay').classList.remove('open');
  $('#paletteInput').value = '';
  if (c.p) go(c.p);
  if (c.a) c.a();
}

$('#cmdkBtn').onclick = () => {
  renderPalette('');
  $('#paletteOverlay').classList.add('open');
  setTimeout(() => $('#paletteInput').focus(), 60);
};
$('#paletteOverlay').onclick = (e) => {
  if (e.target.id === 'paletteOverlay') $('#paletteOverlay').classList.remove('open');
};
$('#paletteInput').oninput = (e) => renderPalette(e.target.value);

document.addEventListener('keydown', (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
    e.preventDefault();
    $('#cmdkBtn').click();
    return;
  }
  if (e.key === 'Escape') {
    $('#paletteOverlay').classList.remove('open');
    if ($('#dialogOverlay').classList.contains('open')) closeDialog(false);
  }
  if (!$('#paletteOverlay').classList.contains('open')) return;
  const items = $('#paletteList')._items || [];
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    palIdx = (palIdx + (e.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length;
    $$('.palette-item').forEach((el, i) => el.classList.toggle('on', i === palIdx));
  }
  if (e.key === 'Enter' && items[palIdx]) execPalette(items[palIdx]);
});

/* ------------------------------------------------------------ render all */

function renderAll() {
  renderStatus();
  renderDashNodes();
  renderSubs();
  renderNodes();
  renderConfig();
  renderLogs();
  renderConns();
  drawChart();
}

// 全站 .switch 由一个 class 驱动，ARIA 状态用同一个观察器统一同步，
// 不必在每个 toggle 调用点各自维护（新增开关自动获得正确的语义）。
new MutationObserver((muts) => {
  for (const m of muts) {
    const el = m.target;
    if (el.classList && el.classList.contains('switch')) {
      el.setAttribute('aria-checked', String(el.classList.contains('on')));
    }
  }
}).observe(document.body, { attributes: true, attributeFilter: ['class'], subtree: true });

/* ------------------------------------------------------------ start */

(function initTheme() {
  const t = document.documentElement.getAttribute('data-theme') || 'dark';
  const icon = document.getElementById('themeIcon');
  if (icon) icon.setAttribute('href', t === 'light' ? '#i-sun' : '#i-moon');
})();

(async function start() {
  // 允许用 ?token= 一次性登录（便于脚本与首次配置），随后从地址栏移除
  const urlTok = new URLSearchParams(location.search).get('token');
  if (urlTok) {
    state.token = urlTok;
    localStorage.setItem('sb_token', urlTok);
    history.replaceState(null, '', location.pathname + location.hash);
  }
  const initial = location.hash.replace('#', '');
  if (PAGE_META[initial]) go(initial);

  // 先询问后端登录状态：needs_setup = 全新面板，强制先设置登录账户密码；
  // login_disabled = 面板内关掉了登录，直接进入首页。旧版本没有该接口时按需要登录处理。
  try {
    const res = await fetch('/api/v1/auth/status');
    if (res.ok) {
      const info = await res.json();
      state.authRequired = info.required !== false;
      state.needsSetup = info.needs_setup === true;
    }
  } catch (e) { /* keep authRequired = true */ }
  $('#logoutBtn').style.display = state.authRequired ? '' : 'none';

  if (state.needsSetup) {
    showSetup();
    return;
  }
  if (!state.authRequired) {
    await boot();
    return;
  }
  if (!state.token) {
    showLogin();
    return;
  }
  try {
    await api('/api/v1/auth/check');
    await boot();
  } catch (e) {
    showLogin();
  }
})();

function applyTheme(theme) {
  document.documentElement.setAttribute('data-theme', theme);
  localStorage.setItem('sb_theme', theme);
  const icon = document.getElementById('themeIcon');
  if (icon) icon.setAttribute('href', theme === 'light' ? '#i-sun' : '#i-moon');
  // 图表颜色来自 CSS 变量，切换后需要重绘
  if (typeof drawChart === 'function' && state.traffic) drawChart();
  if (typeof renderSparklines === 'function') renderSparklines();
}

document.getElementById('themeToggle').onclick = () => {
  const cur = document.documentElement.getAttribute('data-theme') || 'dark';
  applyTheme(cur === 'light' ? 'dark' : 'light');
};

window.closeDialog = closeDialog;
window.__state = state;
