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
}

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

function isApp(r: ResultDto): boolean {
  const ext = extOf(r.name);
  return !r.is_dir && (ext === ".exe" || ext === ".lnk");
}

async function iconUrl(r: ResultDto): Promise<string> {
  const ext = r.is_dir ? "dir" : extOf(r.name);
  const cacheKey = isApp(r) ? "p:" + r.path.toLowerCase() : ext;
  const hit = iconCache.get(cacheKey);
  if (hit !== undefined) return hit;
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
  }
}

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

function highlight(name: string, start: number, len: number): string {
  // match 索引是 UTF-16 单位 = JS 字符串下标，天然对齐
  const s = Math.max(0, Math.min(start, name.length));
  const e = Math.max(s, Math.min(s + len, name.length));
  return `${esc(name.slice(0, s))}<mark>${esc(name.slice(s, e))}</mark>${esc(name.slice(e))}`;
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
  results.innerHTML = items
    .map((r, i) => {
      const badge = r.is_dir ? "文件夹" : extOf(r.name).slice(1).toUpperCase();
      return `<div class="row${i === sel ? " sel" : ""}" data-i="${i}" style="--i:${i}">
        <img data-ext="${esc(extOf(r.name))}" data-dir="${r.is_dir ? 1 : 0}" alt=""/>
        <div class="txt">
          <div class="name">${highlight(r.name, r.match_start, r.match_len)}</div>
          <div class="path">${esc(r.path)}</div>
        </div>
        <span class="badge">${esc(badge)}</span>
      </div>`;
    })
    .join("");
  // 异步填图标
  for (const img of Array.from(results.querySelectorAll("img"))) {
    const el = img as HTMLImageElement;
    const i = Number(el.closest(".row")!.getAttribute("data-i"));
    iconUrl(items[i]).then((url) => {
      if (url) el.src = url;
    });
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

async function doSearch() {
  const query = q.value;
  const seq = ++searchSeq;
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

// —— 键盘导航 ——
window.addEventListener("keydown", (e) => {
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
    invoke("hide_window"); // 先收起，再异步启动目标（目标启动慢也不残留）
    if (e.ctrlKey) {
      invoke("reveal_path", { path: it.path });
    } else {
      invoke("open_path", { path: it.path });
    }
  }
});

// 点击 = 打开
results.addEventListener("click", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>(".row");
  if (!row) return;
  const it = items[Number(row.dataset.i)];
  if (!it) return;
  invoke("hide_window");
  invoke("open_path", { path: it.path });
});

// —— 设置视图（与弹窗同 WebView）——
let settingsMode = false;
let themeSetting = "auto";

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

let hotkeyErrorBound = false;
function listenHotkeyError() {
  if (hotkeyErrorBound) return;
  hotkeyErrorBound = true;
  listen("hotkey-error", ((ev: { payload: string }) => {
    const el = document.getElementById("s-status");
    if (el) el.textContent = ev.payload;
  }) as unknown as EventListener);
}

function exitSettings() {
  settingsMode = false;
  render(true);
}

// 设置入口在托盘右键菜单（设置 → 弹出启动器并切换到设置视图）
listen("settings-view", () => {
  settingsMode = true;
  applyTheme();
  renderSettings();
});

// —— 弹窗显隐动画 ——
await listen("popup-shown", () => {
  panel.classList.add("visible");
  settingsMode = false;
  q.value = "";
  items = [];
  sel = 0;
  refreshHotkeyLabel();
  render(true);
  refreshStatus();
  applyTheme();
  // 多次补聚焦，对抗窗口激活与 WebView 焦点恢复的竞态
  window.setTimeout(() => q.focus(), 30);
  window.setTimeout(() => q.focus(), 150);
  window.setTimeout(() => q.focus(), 400);
});
// 退场：播放收起动画（后端约 160ms 后真正隐藏窗口）
await listen("popup-hiding", () => {
  panel.classList.remove("visible");
});
// 窗口获得焦点时若处于搜索态，确保光标在搜索框
window.addEventListener("focus", () => {
  if (!settingsMode && document.activeElement !== q) {
    window.setTimeout(() => q.focus(), 50);
  }
});

listen("index-ready", ((ev: { payload: { entries: number; startupMs: number; isAdmin: boolean; usnLive: string[]; fromSnapshot: boolean } }) => {
  const p = ev.payload;
  statusEl.textContent = statusText(p.entries, p.usnLive);
}) as unknown as EventListener);

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
