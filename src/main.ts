import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface ResultDto {
  name: string;
  path: string;
  is_dir: boolean;
  score: number;
  match_start: number;
  match_len: number;
  spans: [number, number][];
  /** file | command | audio */
  kind?: string;
  subtitle?: string;
}

/** 自定义命令（与后端 CustomCommand 对应） */
interface CustomCommand {
  id: string;
  name: string;
  keyword: string;
  kind: string;
  command: string;
  args: string[];
  workdir: string;
  admin: boolean;
  hidden: boolean;
  enabled: boolean;
}

const CMD_KINDS: [string, string][] = [
  ["shell", "cmd 命令"],
  ["powershell", "PowerShell"],
  ["exe", "运行程序"],
  ["url", "打开网址"],
  ["builtin", "内置动作"],
];

interface IconDto {
  w: number;
  h: number;
  rgba: number[];
}

const q = document.getElementById("q") as HTMLInputElement;
const results = document.getElementById("results") as HTMLDivElement;
const statusEl = document.getElementById("status") as HTMLSpanElement;
const panel = document.getElementById("panel") as HTMLDivElement;

let items: ResultDto[] = [];
let sel = 0;
let searchSeq = 0;
let staggerTimer: number | undefined;
// 当前呼出快捷键展示文案（空态提示用）
let hotkeyLabel = "双击 Ctrl";

// —— 图标缓存：exe/lnk 按真实路径取应用自身图标，其余按扩展名 ——
const iconCache = new Map<string, string>();
// 同一图标的并发请求合并（快速输入时同一扩展名/路径会被多行同时请求）
const iconInflight = new Map<string, Promise<string>>();

function isUwp(r: ResultDto): boolean {
  return !r.is_dir && r.path.startsWith("shell:AppsFolder\\");
}

function isApp(r: ResultDto): boolean {
  const ext = extOf(r.name);
  return !r.is_dir && (ext === ".exe" || ext === ".lnk" || isUwp(r));
}

/** 结果类型：file（默认）| command | audio */
function kindOf(r: ResultDto): string {
  return r.kind ?? "file";
}

/** 命令 / 音频设备这类"动作型"结果：回车不打开文件，而是执行。 */
function isAction(r: ResultDto): boolean {
  const k = kindOf(r);
  return k === "command" || k === "audio";
}

/** 触发词之后的剩余文本，作为命令的 {arg}（与后端 match_command 的切分一致） */
function pendingArg(): string {
  const parts = q.value.trim().split(/\s+/);
  return parts.length > 1 ? parts.slice(1).join(" ") : "";
}

function iconUrl(r: ResultDto): Promise<string> {
  const ext = r.is_dir ? "dir" : extOf(r.name);
  const cacheKey = isApp(r) ? "p:" + r.path.toLowerCase() : ext;
  const hit = iconCache.get(cacheKey);
  if (hit !== undefined) return Promise.resolve(hit);
  const busy = iconInflight.get(cacheKey);
  if (busy) return busy;
  const p = (async () => {
    try {
      const dto = await invoke<IconDto>("get_icon", {
        key: ext,
        path: isApp(r) ? r.path : "",
      });
      const cv = document.createElement("canvas");
      cv.width = dto.w;
      cv.height = dto.h;
      const ctx = cv.getContext("2d")!;
      ctx.putImageData(
        new ImageData(new Uint8ClampedArray(dto.rgba), dto.w, dto.h),
        0,
        0
      );
      const url = cv.toDataURL();
      iconCache.set(cacheKey, url);
      return url;
    } catch {
      iconCache.set(cacheKey, "");
      return "";
    } finally {
      iconInflight.delete(cacheKey);
    }
  })();
  iconInflight.set(cacheKey, p);
  return p;
}

// 图标懒加载：只给进入视口（提前 120px）的行取图标。
// 结果有 50 行但可见约 8 行，避免每次渲染为整页做图标提取/解码。
const iconObserver = new IntersectionObserver(
  (entries) => {
    for (const e of entries) {
      if (!e.isIntersecting) continue;
      const el = e.target as HTMLImageElement;
      iconObserver.unobserve(el);
      const row = el.closest(".row");
      if (!row) continue;
      const it = items[Number(row.getAttribute("data-i"))];
      if (!it) continue;
      iconUrl(it).then((url) => {
        if (url) el.src = url;
      });
    }
  },
  { root: results, rootMargin: "120px 0px" }
);

function extOf(name: string): string {
  const i = name.lastIndexOf(".");
  return i > 0 ? name.slice(i).toLowerCase() : ".none";
}

// —— 渲染 ——
function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function highlight(name: string, spans: [number, number][]): string {
  // 合并重叠区间后逐段加 <mark>（索引是 UTF-16 单位 = JS 字符串下标，天然对齐）
  const ranges: [number, number][] = [];
  for (const [a, b] of spans) {
    const s = Math.max(0, Math.min(a, name.length));
    const e = Math.max(s, Math.min(b, name.length));
    if (e > s) ranges.push([s, e]);
  }
  if (ranges.length === 0) return esc(name);
  ranges.sort((x, y) => x[0] - y[0]);
  const merged: [number, number][] = [];
  for (const r of ranges) {
    const last = merged[merged.length - 1];
    if (last && r[0] <= last[1]) last[1] = Math.max(last[1], r[1]);
    else merged.push([r[0], r[1]]);
  }
  let out = "";
  let pos = 0;
  for (const [s, e] of merged) {
    out += esc(name.slice(pos, s)) + "<mark>" + esc(name.slice(s, e)) + "</mark>";
    pos = e;
  }
  return out + esc(name.slice(pos));
}

function render(stagger: boolean) {
  if (items.length === 0 && q.value.trim() !== "") {
    results.innerHTML = `<div class="empty"><div>没有找到匹配的文件</div><div class="hint">试试更短的关键词</div></div>`;
    return;
  }
  if (items.length === 0) {
    results.innerHTML = `<div class="empty"><div style="font-size:34px">⌘</div><div>${esc(hotkeyLabel)} 已为你呼出</div><div class="hint">输入即搜，全盘文件毫秒级呈现</div></div>`;
    return;
  }
  results.classList.toggle("stagger", stagger);
  const isRecent = q.value.trim() === "";
  results.innerHTML =
    (isRecent ? `<div class="recent-title">最近使用</div>` : "") +
    items
      .map((r, i) => {
        const uwp = isUwp(r);
        const kind = kindOf(r);
        const action = kind === "command" || kind === "audio";
        const badge = action
          ? kind === "audio"
            ? "音频"
            : "命令"
          : r.is_dir
            ? "文件夹"
            : uwp
              ? "应用"
              : extOf(r.name).slice(1).toUpperCase();
        const nameHtml =
          r.spans && r.spans.length > 0 ? highlight(r.name, r.spans) : esc(r.name);
        const sub = action && r.subtitle ? r.subtitle : uwp ? "系统应用" : r.path;
        const ico = action
          ? `<span class="ico">${kind === "audio" ? "🔊" : "⚡"}</span>`
          : `<img data-ext="${esc(extOf(r.name))}" data-dir="${r.is_dir ? 1 : 0}" alt=""/>`;
        return `<div class="row${i === sel ? " sel" : ""}${action ? " action" : ""}" data-i="${i}" style="--i:${i}">
        ${ico}
        <div class="txt">
          <div class="name">${nameHtml}</div>
          <div class="path">${esc(sub)}</div>
        </div>
        <span class="badge${action ? " badge-cmd" : ""}">${esc(badge)}</span>
      </div>`;
      })
      .join("");
  // 异步填图标（懒加载：仅视口内的行）
  iconObserver.disconnect();
  for (const img of Array.from(results.querySelectorAll("img"))) {
    iconObserver.observe(img);
  }
  if (stagger) {
    window.clearTimeout(staggerTimer);
    staggerTimer = window.setTimeout(() => results.classList.remove("stagger"), 900);
  }
  ensureVisible();
}

function ensureVisible() {
  const row = results.querySelector<HTMLElement>(".row.sel");
  if (row) {
    row.scrollIntoView({ block: "nearest" });
  }
}

/** 仅更新选中态（不重建列表）：高亮平滑滑到目标行 */
function selectRow(next: number) {
  if (next === sel || next < 0 || next >= items.length) return;
  const prevEl = results.querySelector<HTMLElement>(`.row[data-i="${sel}"]`);
  const nextEl = results.querySelector<HTMLElement>(`.row[data-i="${next}"]`);
  prevEl?.classList.remove("sel");
  nextEl?.classList.add("sel");
  sel = next;
  nextEl?.scrollIntoView({ block: "nearest" });
}

async function doSearch(stagger = false) {
  const query = q.value;
  const seq = ++searchSeq;
  // 空查询：展示“最近使用”（呼出即直达常用文件）
  if (query.trim() === "") {
    let rec: ResultDto[] = [];
    try {
      rec = await invoke<ResultDto[]>("recent_items", { limit: 8 });
    } catch {
      rec = [];
    }
    if (seq !== searchSeq || settingsMode || q.value.trim() !== "") return;
    items = rec;
    sel = 0;
    render(stagger);
    refreshStatus();
    return;
  }
  const t0 = performance.now();
  let res: ResultDto[] = [];
  try {
    res = await invoke<ResultDto[]>("search", { query });
  } catch {
    res = [];
  }
  if (seq !== searchSeq) return; // 已被更新的输入取代
  items = res;
  sel = 0;
  render(false);
  const ms = performance.now() - t0;
  if (res.length > 0) {
    statusEl.textContent = `${res.length} 项 · ${ms.toFixed(0)}ms`;
  }
}

/** 刷新呼出快捷键展示文案（空态提示用） */
async function refreshHotkeyLabel() {
  try {
    const label = await invoke<string>("get_hotkey_label");
    if (label && label !== hotkeyLabel) {
      hotkeyLabel = label;
      // 空态正在显示时立即重绘提示文案
      if (items.length === 0 && q.value.trim() === "" && !settingsMode) render(false);
    }
  } catch {
    /* 忽略：保留旧文案 */
  }
}

// —— 输入（防抖 30ms，后端另有代数取消兜底）——
let debounce = 0;
q.addEventListener("input", () => {
  if (settingsMode) settingsMode = false; // 输入即退出设置视图
  window.clearTimeout(debounce);
  debounce = window.setTimeout(doSearch, 30);
});

// —— 右键动作菜单 ——
let ctxEl: HTMLDivElement | null = null;
// 关闭菜单的那次点击不触发“打开”
let ctxGuard = false;

function closeCtxMenu() {
  if (ctxEl) {
    ctxEl.remove();
    ctxEl = null;
  }
}

interface CtxEntry {
  label: string;
  hint?: string;
  run: () => void;
}

function buildCtxEntries(it: ResultDto): (CtxEntry | "sep")[] {
  // 命令 / 音频设备：只有"执行"一种语义
  if (isAction(it)) {
    return [
      { label: "执行", hint: "↵", run: () => { void activate(it); } },
      "sep",
      { label: "复制名称", run: () => copyFlash(it.name, false) },
    ];
  }
  const ext = extOf(it.name);
  const uwp = isUwp(it);
  const isApp =
    !it.is_dir &&
    (ext === ".exe" || ext === ".lnk" || ext === ".bat" || ext === ".cmd" || ext === ".msi");
  const hide = () => invoke("hide_window");
  const entries: (CtxEntry | "sep")[] = [
    { label: "打开", hint: "↵", run: () => { hide(); invoke("open_path", { path: it.path }); } },
  ];
  if (!uwp) {
    entries.push({ label: "打开所在文件夹", hint: "Ctrl ↵", run: () => { hide(); invoke("reveal_path", { path: it.path }); } });
  }
  entries.push("sep", { label: "复制路径", run: () => copyFlash(it.path, false) });
  if (!uwp) {
    entries.push({ label: "复制文件", run: () => copyFlash(it.path, true) });
  }
  entries.push(
    "sep",
    { label: "在 cmd 中打开", run: () => { hide(); invoke("open_in_terminal", { path: it.path, kind: "cmd" }); } },
    { label: "在 PowerShell 中打开", run: () => { hide(); invoke("open_in_terminal", { path: it.path, kind: "powershell" }); } },
    { label: "在 cmd 中打开（管理员）", run: () => { hide(); invoke("open_in_terminal", { path: it.path, kind: "cmd_admin" }); } },
    { label: "在 PowerShell 中打开（管理员）", run: () => { hide(); invoke("open_in_terminal", { path: it.path, kind: "powershell_admin" }); } },
  );
  const extra: CtxEntry[] = [];
  if (isApp) {
    extra.push({ label: "以管理员身份运行", run: () => { hide(); invoke("run_as_admin", { path: it.path }); } });
  }
  if (!it.is_dir && !uwp) {
    extra.push({ label: "打开方式…", run: () => { hide(); invoke("open_with_dialog", { path: it.path }); } });
  }
  if (extra.length > 0) entries.push("sep", ...extra);
  entries.push("sep", { label: "属性", run: () => { hide(); invoke("show_properties", { path: it.path }); } });
  return entries;
}

async function copyFlash(path: string, asFile: boolean) {
  try {
    await invoke(asFile ? "copy_file" : "copy_path", { path });
    flashStatus(asFile ? "已复制文件，可在资源管理器粘贴" : "已复制路径");
  } catch {
    flashStatus("复制失败");
  }
}

let flashTimer = 0;
function flashStatus(text: string) {
  statusEl.textContent = text;
  window.clearTimeout(flashTimer);
  flashTimer = window.setTimeout(() => refreshStatus(), 1800);
}

/**
 * 执行一条结果。
 * - 文件 / 文件夹 / UWP：ShellExecute 打开
 * - 自定义命令、音频设备：交给后端 run_custom
 *
 * 命令失败时**不隐藏窗口**，让用户看到失败原因（例如取消了 UAC）。
 */
async function activate(it: ResultDto): Promise<void> {
  if (!isAction(it)) {
    invoke("hide_window");
    invoke("open_path", { path: it.path });
    return;
  }
  try {
    const msg = await invoke<string>("run_custom", {
      id: it.path,
      arg: pendingArg(),
    });
    flashStatus(typeof msg === "string" && msg ? msg : "已执行");
    invoke("hide_window");
  } catch (e) {
    flashStatus(String(e));
  }
}

function showCtxMenu(x: number, y: number, it: ResultDto, idx: number) {
  closeCtxMenu();
  selectRow(idx);
  const el = document.createElement("div");
  el.className = "ctx";
  for (const ent of buildCtxEntries(it)) {
    if (ent === "sep") {
      const sep = document.createElement("div");
      sep.className = "ctx-sep";
      el.appendChild(sep);
      continue;
    }
    const rowEl = document.createElement("div");
    rowEl.className = "ctx-item";
    rowEl.innerHTML = `<span>${esc(ent.label)}</span>${ent.hint ? `<span class="ctx-hint">${esc(ent.hint)}</span>` : ""}`;
    rowEl.addEventListener("click", () => {
      closeCtxMenu();
      ent.run();
    });
    el.appendChild(rowEl);
  }
  document.body.appendChild(el);
  // 边界钳制（距窗口边缘 8px）
  const r = el.getBoundingClientRect();
  const px = Math.max(8, Math.min(x, window.innerWidth - r.width - 8));
  const py = Math.max(8, Math.min(y, window.innerHeight - r.height - 8));
  el.style.left = `${px}px`;
  el.style.top = `${py}px`;
  ctxEl = el;
}

// 全应用禁止系统右键菜单；结果行上弹出动作菜单
document.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  const target = e.target as HTMLElement;
  if (target.closest(".ctx")) return;
  const row = target.closest<HTMLElement>(".row");
  if (!row || settingsMode) {
    closeCtxMenu();
    return;
  }
  const idx = Number(row.dataset.i);
  const it = items[idx];
  if (!it) return;
  showCtxMenu(e.clientX, e.clientY, it, idx);
});

// 点击菜单以外区域：关闭菜单，且不误触“打开”
window.addEventListener("mousedown", (e) => {
  if (ctxEl && !(e.target as HTMLElement).closest(".ctx")) {
    closeCtxMenu();
    ctxGuard = true;
  } else if (!ctxEl) {
    ctxGuard = false;
  }
});

// —— 键盘导航 ——
window.addEventListener("keydown", (e) => {
  if (ctxEl) {
    if (e.key === "Escape") {
      e.preventDefault();
      closeCtxMenu();
      return;
    }
    // 其他按键：先关菜单，再按原逻辑处理
    closeCtxMenu();
  }
  if (e.key === "Escape") {
    if (settingsMode) {
      exitSettings();
    } else {
      invoke("hide_window");
    }
    return;
  }
  if (e.key === "ArrowDown") {
    if (settingsMode) return;
    e.preventDefault();
    selectRow(sel + 1);
  } else if (e.key === "ArrowUp") {
    if (settingsMode) return;
    e.preventDefault();
    selectRow(sel - 1);
  } else if (e.key === "Enter") {
    if (settingsMode) return;
    e.preventDefault();
    const it = items[sel];
    if (!it) return;
    if (e.ctrlKey && !isAction(it)) {
      invoke("hide_window"); // 先收起，再异步启动目标（目标启动慢也不残留）
      invoke("reveal_path", { path: it.path });
    } else {
      void activate(it);
    }
  } else if (e.altKey && !settingsMode) {
    // Alt+1~9：直接打开第 N 条结果
    const m = /^(?:Digit|Numpad)([1-9])$/.exec(e.code);
    const it = m ? items[Number(m[1]) - 1] : undefined;
    if (it) {
      e.preventDefault();
      void activate(it);
    }
  }
});

// 点击 = 打开（刚关掉右键菜单的那次点击不触发）
results.addEventListener("click", (e) => {
  if (ctxGuard) {
    ctxGuard = false;
    return;
  }
  const row = (e.target as HTMLElement).closest<HTMLElement>(".row");
  if (!row) return;
  const it = items[Number(row.dataset.i)];
  if (!it) return;
  void activate(it);
});

// —— 设置视图（与弹窗同 WebView）——
let settingsMode = false;
let themeSetting = "auto";

// 自定义命令的编辑状态（设置页内嵌管理）
let cmdList: CustomCommand[] = [];
let cmdEditing: CustomCommand | null = null;
let cmdJsonMode = false;
let cmdJsonText = "";

interface SettingsPayload {
  autostart_registered: boolean;
  volumes: { letter: string; is_ntfs: boolean; entries: number; enabled: boolean }[];
  autostart: boolean;
  double_ctrl: boolean;
  hotkey_mode: string;
  custom_hotkey: string;
  theme: string;
  disabled_volumes: string[];
  excluded_dirs: string[];
}

async function applyTheme() {
  let t = themeSetting;
  if (t !== "dark" && t !== "light") {
    try {
      t = await invoke<string>("get_windows_theme");
    } catch {
      t = "dark";
    }
  }
  document.documentElement.dataset.theme = t === "light" ? "light" : "dark";
}

async function renderSettings() {
  results.innerHTML = `<div class="settings-view"><div class="settings-loading">加载中…</div></div>`;
  let p: SettingsPayload;
  try {
    p = await invoke<SettingsPayload>("get_settings");
  } catch (e) {
    results.innerHTML = `<div class="settings-view"><div class="settings-loading">加载失败: ${esc(String(e))}</div></div>`;
    return;
  }
  if (!settingsMode) return; // 期间已切回搜索
  // 自定义命令列表（独立于 settings.json，便于导出分享）
  try {
    cmdList = await invoke<CustomCommand[]>("list_custom_commands");
  } catch {
    cmdList = [];
  }
  if (!Array.isArray(cmdList)) cmdList = [];
  cmdEditing = null;
  cmdJsonMode = false;
  results.classList.remove("stagger");
  results.innerHTML = `<div class="settings-view">
    <label class="opt"><input type="checkbox" id="s-autostart" ${p.autostart ? "checked" : ""}/> 开机自启</label>
    <div class="opt-title">呼出快捷键</div>
    <div class="opt-row">
      <select id="s-hk-mode">
        <option value="double_ctrl" ${p.hotkey_mode !== "custom" && p.hotkey_mode !== "alt_space" ? "selected" : ""}>双击 Ctrl</option>
        <option value="alt_space" ${p.hotkey_mode === "alt_space" ? "selected" : ""}>Alt + 空格</option>
        <option value="custom" ${p.hotkey_mode === "custom" ? "selected" : ""}>自定义快捷键</option>
      </select>
      <input id="s-hk" readonly placeholder="切到“自定义”后按下组合键" value="${
        p.hotkey_mode === "custom" ? esc(p.custom_hotkey) : ""
      }"/>
    </div>
    <div class="opt-hint">下拉选“自定义”，再点右侧框按下组合键（Alt+空格 可能与系统菜单冲突，推荐 Alt+Q、Ctrl+Alt+空格 等）。</div>
    <div class="opt-title">主题</div>
    <div class="opt-row">
      <select id="s-theme">
        <option value="auto" ${p.theme === "auto" || p.theme === "" ? "selected" : ""}>跟随 Windows</option>
        <option value="dark" ${p.theme === "dark" ? "selected" : ""}>深色</option>
        <option value="light" ${p.theme === "light" ? "selected" : ""}>浅色</option>
      </select>
    </div>
    <div class="opt-title">磁盘</div>
    <div id="s-volumes">${p.volumes
      .map(
        (v) => `<label class="opt sub"><input type="checkbox" data-letter="${v.letter}" ${
          v.enabled ? "checked" : ""
        }/> ${v.letter}:  ${v.is_ntfs ? "NTFS" : "目录遍历"} · ${v.entries.toLocaleString()} 项</label>`
      )
      .join("")}</div>
    <div class="opt-title">排除目录</div>
    <div class="opt-hint">每行一个绝对路径，命中目录下的文件不出现在搜索结果。</div>
    <textarea id="s-excluded" rows="4" spellcheck="false">${esc(p.excluded_dirs.join("\n"))}</textarea>
    <div class="opt-title">自定义命令</div>
    <div class="opt-hint">加一条命令后，在搜索框里输入它的触发词就能直接执行。命令存在单独的 commands.json，可整份导出分享给别人。</div>
    <div id="s-cmds" class="cmd-box"></div>
    <div class="settings-actions">
      <button id="s-save">保存</button>
      <span id="s-status"></span>
      <span class="opt-hint">Esc 返回搜索</span>
    </div>
  </div>`;

  // 快捷键捕获：捕获成功自动切到“自定义”模式，避免忘了切下拉框
  const hkInput = document.getElementById("s-hk") as HTMLInputElement;
  let hotkeyValue = hkInput.value.trim();
  const hkMode = document.getElementById("s-hk-mode") as HTMLSelectElement;
  const MODIFIER_KEYS = ["Control", "Alt", "Shift", "Meta"];
  hkInput.addEventListener("keydown", (e) => {
    if (e.key === "Escape") return; // 让 Esc 正常返回搜索
    e.preventDefault();
    e.stopPropagation();
    // 只按了修饰键：提示继续，不当作完整快捷键
    if (MODIFIER_KEYS.includes(e.key)) {
      hkInput.value = "再按一个普通键完成设置（如空格、字母、数字）";
      return;
    }
    const mods: string[] = [];
    if (e.ctrlKey) mods.push("ctrl");
    if (e.altKey) mods.push("alt");
    if (e.shiftKey) mods.push("shift");
    if (e.metaKey) mods.push("win");
    if (mods.length === 0) {
      hkInput.value = "需要至少一个修饰键（Ctrl / Alt / Shift / Win）";
      return;
    }
    let k = e.key;
    if (/^[a-z]$/i.test(k)) k = k.toUpperCase();
    else if (/^F\d{1,2}$/.test(k)) k = k.toUpperCase();
    else if (k === " ") k = "space";
    else k = k.toLowerCase();
    hotkeyValue = [...mods, k].join("+").toLowerCase();
    hkInput.value = hotkeyValue;
    hkMode.value = "custom"; // 捕获即启用自定义模式
  });
  const syncHk = () => {
    hkInput.disabled = hkMode.value !== "custom";
  };
  hkMode.addEventListener("change", syncHk);
  syncHk();

  // 主题即时预览
  (document.getElementById("s-theme") as HTMLSelectElement).addEventListener("change", (e) => {
    themeSetting = (e.target as HTMLSelectElement).value;
    applyTheme();
  });

  // 自定义命令管理区
  renderCommandSection();

  document.getElementById("s-save")!.addEventListener("click", async () => {
    const disabled = Array.from(
      results.querySelectorAll<HTMLInputElement>("#s-volumes input[data-letter]")
    )
      .filter((el) => !el.checked)
      .map((el) => el.dataset.letter ?? "");
    const dto = {
      autostart: (document.getElementById("s-autostart") as HTMLInputElement).checked,
      double_ctrl: (document.getElementById("s-hk-mode") as HTMLSelectElement).value !== "custom",
      hotkey_mode: (document.getElementById("s-hk-mode") as HTMLSelectElement).value,
      custom_hotkey: hotkeyValue,
      theme: (document.getElementById("s-theme") as HTMLSelectElement).value,
      disabled_volumes: disabled,
      excluded_dirs: (document.getElementById("s-excluded") as HTMLTextAreaElement).value
        .split("\n")
        .map((s) => s.trim())
        .filter(Boolean),
    };
    const status = document.getElementById("s-status")!;
    status.textContent = "保存中…";
    try {
      await invoke("set_settings", { settings: dto });
      themeSetting = dto.theme;
      refreshHotkeyLabel();
      status.textContent = "已保存";
      listenHotkeyError();
      window.setTimeout(() => (status.textContent = ""), 2500);
    } catch (e) {
      status.textContent = `保存失败: ${e}`;
    }
  });
}

// —— 自定义命令管理（设置页内嵌） ——

function cmdKindLabel(kind: string): string {
  const hit = CMD_KINDS.find((k) => k[0] === kind);
  return hit ? hit[1] : kind;
}

function cmdSummary(c: CustomCommand): string {
  const body =
    c.kind === "exe" && c.args.length > 0 ? `${c.command} ${c.args.join(" ")}` : c.command;
  const kw = c.keyword.trim() ? `触发词 ${c.keyword} · ` : "";
  return `${kw}${cmdKindLabel(c.kind)}${body ? " · " + body : ""}`;
}

function setCmdStatus(text: string) {
  const el = document.getElementById("cmd-status");
  if (el) el.textContent = text;
}

async function persistCommands(): Promise<boolean> {
  try {
    await invoke("save_custom_commands", { commands: cmdList });
    return true;
  } catch (e) {
    setCmdStatus(`保存失败: ${e}`);
    return false;
  }
}

function renderCommandSection() {
  const box = document.getElementById("s-cmds");
  if (!box) return;
  const rows = cmdList
    .map(
      (c, i) => `<div class="cmd-row">
      <input type="checkbox" data-cmd-enable="${i}" ${c.enabled ? "checked" : ""} title="启用/停用"/>
      <div class="cmd-main">
        <div class="cmd-name">${esc(c.name || "(未命名)")}</div>
        <div class="cmd-sub">${esc(cmdSummary(c))}</div>
      </div>
      <button class="mini" data-cmd-edit="${i}">编辑</button>
      <button class="mini" data-cmd-del="${i}">删除</button>
    </div>`
    )
    .join("");
  box.innerHTML = `
    <div class="cmd-list">${
      rows || `<div class="opt-hint">还没有自定义命令，点“新增命令”添加一条。</div>`
    }</div>
    <div class="opt-row">
      <button class="mini" id="cmd-add">新增命令</button>
      <button class="mini" id="cmd-json">导入 / 导出 JSON</button>
      <span id="cmd-status" class="opt-hint"></span>
    </div>
    <div id="cmd-editor"></div>
    <div id="cmd-json-box"></div>`;

  box.querySelectorAll<HTMLInputElement>("input[data-cmd-enable]").forEach((el) => {
    el.addEventListener("change", async () => {
      const i = Number(el.dataset.cmdEnable);
      if (!cmdList[i]) return;
      cmdList[i].enabled = el.checked;
      await persistCommands();
    });
  });
  box.querySelectorAll<HTMLButtonElement>("button[data-cmd-edit]").forEach((el) => {
    el.addEventListener("click", () => {
      const i = Number(el.dataset.cmdEdit);
      const src = cmdList[i];
      if (!src) return;
      cmdEditing = { ...src, args: [...src.args] };
      cmdJsonMode = false;
      renderCommandSection();
    });
  });
  box.querySelectorAll<HTMLButtonElement>("button[data-cmd-del]").forEach((el) => {
    el.addEventListener("click", async () => {
      const i = Number(el.dataset.cmdDel);
      const name = cmdList[i]?.name ?? "";
      cmdList.splice(i, 1);
      cmdEditing = null;
      await persistCommands();
      renderCommandSection();
      setCmdStatus(`已删除 ${name}`);
    });
  });
  document.getElementById("cmd-add")!.addEventListener("click", () => {
    cmdEditing = {
      id: "",
      name: "",
      keyword: "",
      kind: "shell",
      command: "",
      args: [],
      workdir: "",
      admin: false,
      hidden: true,
      enabled: true,
    };
    cmdJsonMode = false;
    renderCommandSection();
  });
  document.getElementById("cmd-json")!.addEventListener("click", () => {
    cmdJsonMode = !cmdJsonMode;
    if (cmdJsonMode) cmdJsonText = JSON.stringify(cmdList, null, 2);
    cmdEditing = null;
    renderCommandSection();
  });

  if (cmdEditing) renderCommandEditor();
  if (cmdJsonMode) renderCommandJsonBox();
}

function renderCommandEditor() {
  const box = document.getElementById("cmd-editor");
  if (!box || !cmdEditing) return;
  const c = cmdEditing;
  const isNew = !cmdList.some((x) => x.id === c.id);
  box.innerHTML = `
    <div class="cmd-form">
      <div class="opt-row">
        <label class="opt sub">名称<input id="cf-name" value="${esc(c.name)}" placeholder="切换音频设备"/></label>
        <label class="opt sub">触发词<input id="cf-kw" value="${esc(c.keyword)}" placeholder="空格分隔，如：音频 yp"/></label>
      </div>
      <div class="opt-row">
        <label class="opt sub">类型<select id="cf-kind">${CMD_KINDS.map(
          (k) => `<option value="${k[0]}" ${c.kind === k[0] ? "selected" : ""}>${esc(k[1])}</option>`
        ).join("")}</select></label>
        <label class="opt sub">工作目录<input id="cf-dir" value="${esc(c.workdir)}" placeholder="留空 = 默认"/></label>
      </div>
      <label class="opt sub">命令内容<input id="cf-cmd" value="${esc(c.command)}" placeholder="nircmd setdefaultsounddevice 耳机"/></label>
      <label class="opt sub">参数<input id="cf-args" value="${esc(c.args.join(" "))}" placeholder="仅“运行程序”用；空格分隔，支持 {arg}"/></label>
      <label class="opt sub"><input type="checkbox" id="cf-admin" ${c.admin ? "checked" : ""}/> 以管理员身份运行（会弹 UAC）</label>
      <label class="opt sub"><input type="checkbox" id="cf-hidden" ${c.hidden ? "checked" : ""}/> 隐藏控制台黑框</label>
      <div class="opt-hint">占位符 {arg} = 触发词之后剩下的文字。例：触发词 yt、类型“运行程序”、命令 mpv.exe、参数 {arg}，输入“yt 猫和老鼠”就会执行 mpv.exe 猫和老鼠。</div>
      <div class="opt-row">
        <button id="cf-save">${isNew ? "添加" : "保存修改"}</button>
        <button class="mini" id="cf-cancel">取消</button>
      </div>
    </div>`;

  document.getElementById("cf-save")!.addEventListener("click", async () => {
    const val = (id: string) =>
      (document.getElementById(id) as HTMLInputElement).value.trim();
    const name = val("cf-name");
    if (!name) {
      setCmdStatus("请先填写名称");
      return;
    }
    const updated: CustomCommand = {
      id: c.id || `c${Date.now()}`,
      name,
      keyword: val("cf-kw"),
      kind: (document.getElementById("cf-kind") as HTMLSelectElement).value,
      command: val("cf-cmd"),
      args: val("cf-args").split(/\s+/).filter(Boolean),
      workdir: val("cf-dir"),
      admin: (document.getElementById("cf-admin") as HTMLInputElement).checked,
      hidden: (document.getElementById("cf-hidden") as HTMLInputElement).checked,
      enabled: c.enabled,
    };
    const idx = cmdList.findIndex((x) => x.id === updated.id);
    if (idx >= 0) cmdList[idx] = updated;
    else cmdList.push(updated);
    cmdEditing = null;
    const ok = await persistCommands();
    renderCommandSection();
    if (ok) setCmdStatus("已保存，回搜索框输入触发词即可用");
  });
  document.getElementById("cf-cancel")!.addEventListener("click", () => {
    cmdEditing = null;
    renderCommandSection();
  });
}

function renderCommandJsonBox() {
  const box = document.getElementById("cmd-json-box");
  if (!box || !cmdJsonMode) return;
  box.innerHTML = `
    <div class="opt-hint">把这段 JSON 复制给别人，对方粘贴后点“导入覆盖”就能得到完全一样的命令集。</div>
    <textarea id="cmd-json" rows="8" spellcheck="false">${esc(cmdJsonText)}</textarea>
    <div class="opt-row">
      <button class="mini" id="cmd-json-current">填入当前配置</button>
      <button class="mini" id="cmd-json-import">导入覆盖</button>
      <button class="mini" id="cmd-json-copy">复制到剪贴板</button>
    </div>`;
  const ta = document.getElementById("cmd-json") as HTMLTextAreaElement;
  ta.addEventListener("input", () => {
    cmdJsonText = ta.value;
  });
  document.getElementById("cmd-json-current")!.addEventListener("click", () => {
    cmdJsonText = JSON.stringify(cmdList, null, 2);
    renderCommandJsonBox();
  });
  document.getElementById("cmd-json-copy")!.addEventListener("click", async () => {
    try {
      await invoke("copy_path", { path: cmdJsonText });
      setCmdStatus("JSON 已复制到剪贴板");
    } catch (e) {
      setCmdStatus(`复制失败: ${e}`);
    }
  });
  document.getElementById("cmd-json-import")!.addEventListener("click", async () => {
    let parsed: unknown;
    try {
      parsed = JSON.parse(cmdJsonText);
    } catch {
      setCmdStatus("JSON 格式错误，检查一下括号和逗号");
      return;
    }
    if (!Array.isArray(parsed)) {
      setCmdStatus("JSON 顶层必须是一个数组");
      return;
    }
    cmdList = (parsed as CustomCommand[]).map((c) => ({
      ...c,
      args: Array.isArray(c?.args) ? c.args : [],
    }));
    const n = cmdList.length;
    const ok = await persistCommands();
    renderCommandSection();
    if (ok) setCmdStatus(`已导入 ${n} 条命令`);
  });
}

let hotkeyErrorBound = false;
function listenHotkeyError() {
  if (hotkeyErrorBound) return;
  hotkeyErrorBound = true;
  listen<string>("hotkey-error", (ev) => {
    const el = document.getElementById("s-status");
    if (el) el.textContent = ev.payload;
  });
}

function exitSettings() {
  settingsMode = false;
  void doSearch(true);
}

// 设置入口在托盘右键菜单（设置 → 弹出启动器并切换到设置视图）
listen("settings-view", () => {
  settingsMode = true;
  applyTheme();
  renderSettings();
});

// —— 弹窗显隐动画 ——
await listen("popup-shown", () => {
  closeCtxMenu();
  panel.classList.add("visible");
  settingsMode = false;
  q.value = "";
  items = [];
  sel = 0;
  refreshHotkeyLabel();
  void doSearch(true); // 空查询 → 载入“最近使用”（带交错动画）
  refreshStatus();
  applyTheme();
  // 多次补聚焦，对抗窗口激活与 WebView 焦点恢复的竞态
  window.setTimeout(() => q.focus(), 30);
  window.setTimeout(() => q.focus(), 150);
  window.setTimeout(() => q.focus(), 400);
});
// 退场：播放收起动画（后端约 160ms 后真正隐藏窗口）
await listen("popup-hiding", () => {
  closeCtxMenu();
  panel.classList.remove("visible");
});
// Alt+1~9 数字直达（钩子层拦截后转发；WebView2 输入链路对 Alt+数字 不可靠）
await listen<number>("quick-open", (ev) => {
  if (settingsMode) return;
  const it = items[ev.payload - 1];
  if (!it) return;
  void activate(it);
});
// 窗口获得焦点时若处于搜索态，确保光标在搜索框
window.addEventListener("focus", () => {
  if (!settingsMode && document.activeElement !== q) {
    window.setTimeout(() => q.focus(), 50);
  }
});

interface IndexReadyPayload {
  entries: number;
  startupMs: number;
  isAdmin: boolean;
  usnLive: string[];
  fromSnapshot: boolean;
}

listen<IndexReadyPayload>("index-ready", (ev) => {
  const p = ev.payload;
  statusEl.textContent = statusText(p.entries, p.usnLive);
});

function statusText(entries: number, usnLive: string[]): string {
  const mode = usnLive && usnLive.length > 0 ? "MFT 实时" : "遍历快照";
  return `已索引 ${entries.toLocaleString()} 项 · ${mode}`;
}

async function refreshStatus() {
  try {
    const s = await invoke<{
      entries: number;
      isAdmin: boolean;
      usnLive: string[];
      ready: boolean;
    }>("engine_status");
    if (s.ready) {
      statusEl.textContent = statusText(s.entries, s.usnLive);
    } else {
      statusEl.textContent = "索引构建中…";
    }
  } catch {
    /* ignore */
  }
}
refreshStatus();
refreshHotkeyLabel();
