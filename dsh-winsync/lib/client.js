/**
 * dsh-winsync — browser half.
 *
 * 主区面板（`main` 槽，key=winsync）+ 左侧栏图标行（`sidebar.panellist`）。
 * 版式照 exe：180px 侧栏 + 任务 / 日志 / 设置三页；数据走 `/winsync/api`，
 * 实时更新走 SSE（`/winsync/api/events`）——事件只做增量，`/state` 才是权威快照。
 *
 * 手写 bundle：`window.__ModuleLoader__.load({id, factory})`，只依赖基线 `react`。
 */

window.__ModuleLoader__.load({
  id: 'dsh-winsync',
  factory: (require) => {
    const React = require('react')
    const h = React.createElement

    const PANEL_ID = 'winsync'
    const API = '/winsync/api'
    const MB = 1024 * 1024
    const LOG_RING = 400

    /* ------------------------------------------------------------ 样式 */

    const CSS = `
/* 主题变量：默认深色；浅色主题由宿主 body[data-ds-dark-theme] 缺省时切过来。
   换肤只改这一组变量，控件样式本身不用重复写。 */
.ws-root{
  --ws-bg:#0E0F10; --ws-side:#161719; --ws-card:#1C1D1F; --ws-input:#1A1B1D;
  --ws-zebra:#121315; --ws-hover:#232427; --ws-chip:#26282B;
  --ws-line:#2A2C2E; --ws-line-soft:#202225; --ws-line-strong:#33363A;
  --ws-text:#EDEDEE; --ws-text-soft:#C9CBCF; --ws-dim:#A0A2A6;
  --ws-muted:#8B8E94; --ws-faint:#6E7176; --ws-chip-text:#B9BBBF;
  --ws-active:#1E355C; --ws-accent:#2B6CF6; --ws-accent-hover:#3B78F8;
  --ws-ok:#4CC38A; --ws-warn:#E6A23C; --ws-danger:#F0554D;
  --ws-danger-line:#4A2320; --ws-danger-bg:#2A1715;
}
.ws-root[data-theme="light"]{
  --ws-bg:#F5F6F8; --ws-side:#FFFFFF; --ws-card:#FFFFFF; --ws-input:#FFFFFF;
  --ws-zebra:#FAFBFC; --ws-hover:#EFF1F4; --ws-chip:#EEF0F3;
  --ws-line:#E1E4E9; --ws-line-soft:#EDEFF3; --ws-line-strong:#CFD3D9;
  --ws-text:#1B1D21; --ws-text-soft:#3D4147; --ws-dim:#5A5F66;
  --ws-muted:#767C83; --ws-faint:#9AA0A7; --ws-chip-text:#4A4F55;
  --ws-active:#DCE8FF; --ws-accent:#2B6CF6; --ws-accent-hover:#1F5CE0;
  --ws-ok:#1F8A5B; --ws-warn:#A86800; --ws-danger:#C0352D;
  --ws-danger-line:#E7C3BF; --ws-danger-bg:#FCEFEE;
}

/* 主区面板只能占 dsh 给它的那一格：不能用 absolute/inset:0——那会以 .frame
   为包含块，连左侧栏和 Windows 标题栏一起盖住（宿主 .frame 只靠 padding-top
   让出标题栏，absolute 元素贴的是 padding box 边缘，正好压上去）。宿主把
   main 槽塞在 display:contents 的包装里，所以这里直接作为 .mainPanel 的
   flex 子项撑满即可。 */
.ws-root{position:relative;flex:1 1 auto;min-width:0;min-height:0;height:100%;
  box-sizing:border-box;display:flex;background:var(--ws-bg);color:var(--ws-text);
  font:13px/1.55 ui-sans-serif,system-ui,"Segoe UI","Microsoft YaHei",sans-serif;overflow:hidden;
  container-type:inline-size}
.ws-side{width:180px;flex:0 0 180px;background:var(--ws-side);border-right:1px solid var(--ws-line);
  display:flex;flex-direction:column;padding:14px 12px 12px}
.ws-brand{display:flex;align-items:center;gap:8px;padding:2px 4px 0}
/* 品牌位只留一个线条图标，和 dsh 自己的侧栏图标同一种做法：无底色块、
   currentColor、18px、1.5 描边。之前是蓝色圆角块 + 白字 W，太像第三方 App 的
   logo，放在 dsh 的灰度图标里很跳。 */
.ws-mark{display:flex;align-items:center;justify-content:center;color:var(--ws-text-soft);
  padding:1px 0}
.ws-brand-name{font-size:15px;font-weight:600;color:var(--ws-text)}
.ws-nav{margin-top:18px;display:flex;flex-direction:column;gap:2px}
.ws-nav-item{display:flex;align-items:center;gap:8px;height:32px;padding:0 8px;border:0;
  border-radius:6px;background:transparent;color:var(--ws-dim);font:inherit;font-size:13px;
  cursor:pointer;text-align:left;width:100%}
.ws-nav-item:hover{background:var(--ws-hover);color:var(--ws-text)}
.ws-nav-item[data-active="true"]{background:var(--ws-active);color:var(--ws-text)}
.ws-nav-item:focus,.ws-btn:focus,.ws-icon-btn:focus{outline:none}
.ws-nav-item:focus-visible,.ws-btn:focus-visible,.ws-icon-btn:focus-visible{
  outline:2px solid var(--ws-accent);outline-offset:1px}
.ws-side-foot{margin-top:auto;padding-top:10px;border-top:1px solid var(--ws-line-soft)}
/* 侧栏底部的版本角标：装的是哪一版、哪个提交，一眼可见。
   默认压得很淡（它不是操作，是标识），hover/聚焦才提亮；点了复制完整信息。 */
.ws-ver{margin-top:8px;display:flex;align-items:center;gap:5px;width:100%;padding:3px 8px;
  border:0;border-radius:5px;background:transparent;color:var(--ws-faint);font:inherit;
  font-size:11px;line-height:1.4;text-align:left;cursor:pointer;
  font-family:ui-monospace,Consolas,monospace;white-space:nowrap;overflow:hidden;
  text-overflow:ellipsis}
.ws-ver:hover{background:var(--ws-hover);color:var(--ws-dim)}
.ws-ver[data-dirty="true"]{color:#C9A227}
.ws-ver[data-dirty="true"]:hover{color:#E0B73A}
.ws-ver:focus{outline:none}
.ws-ver:focus-visible{outline:2px solid var(--ws-accent);outline-offset:1px}
/* 配对状态：ok=两端已在同一房间，bad=进房间失败（房间满/地址错/中继要求密钥） */
.ws-pair-title{font-weight:600}
.ws-pair-title[data-presence="ok"]{color:var(--ws-ok)}
.ws-pair-title[data-presence="bad"]{color:var(--ws-danger)}
.ws-content{flex:1;min-width:0;display:flex;flex-direction:column;overflow:hidden}
.ws-scroll{overflow:auto;padding:18px 24px 32px;flex:1}
.ws-scroll::-webkit-scrollbar,.ws-raw-body::-webkit-scrollbar{width:8px;height:8px}
.ws-scroll::-webkit-scrollbar-thumb,.ws-raw-body::-webkit-scrollbar-thumb{
  background:var(--ws-line);border-radius:4px}
.ws-scroll::-webkit-scrollbar-thumb:hover,.ws-raw-body::-webkit-scrollbar-thumb:hover{
  background:#3A3D41}
.ws-scroll::-webkit-scrollbar-track,.ws-raw-body::-webkit-scrollbar-track{background:transparent}
.ws-row{display:flex;align-items:center;gap:10px}
.ws-page-head{display:flex;align-items:center;justify-content:space-between;margin-bottom:4px}
.ws-title{font-size:18px;font-weight:600;margin:0}
.ws-sub{color:var(--ws-muted);font-size:12px}
.ws-device{color:var(--ws-dim);font-size:12px;margin:8px 0 14px}
.ws-btn{appearance:none;border:1px solid var(--ws-line);background:var(--ws-hover);color:var(--ws-text);
  border-radius:6px;padding:5px 12px;font:inherit;font-size:12px;cursor:pointer}
.ws-btn:hover{background:var(--ws-line)}
.ws-btn[disabled]{opacity:.45;cursor:not-allowed}
.ws-btn-primary{background:var(--ws-accent);border-color:var(--ws-accent);color:#fff}
.ws-btn-primary:hover{background:var(--ws-accent-hover)}
.ws-btn-ghost{background:transparent}
.ws-btn-danger{color:var(--ws-danger);border-color:var(--ws-danger-line)}
.ws-btn-danger:hover{background:var(--ws-danger-bg)}
.ws-card{background:var(--ws-card);border:1px solid var(--ws-line);border-radius:10px;padding:14px 16px;
  margin-bottom:12px}
.ws-card[data-selected="true"]{border-color:var(--ws-accent)}
.ws-card-head{display:flex;align-items:flex-start;gap:10px}
.ws-folder{width:30px;height:30px;border-radius:8px;background:var(--ws-chip);color:var(--ws-chip-text);
  display:flex;align-items:center;justify-content:center;flex:0 0 30px}
.ws-task-name{font-size:15px;font-weight:600}
.ws-task-state{font-size:12px}
.ws-card-actions{margin-left:auto;display:flex;align-items:center;gap:6px}
.ws-icon-btn{width:26px;height:26px;border-radius:13px;border:1px solid var(--ws-line);
  background:transparent;color:var(--ws-dim);cursor:pointer;display:flex;align-items:center;
  justify-content:center;font-size:13px;line-height:1;padding:0}
.ws-icon-btn:hover{background:var(--ws-hover);color:var(--ws-text)}
.ws-menu{position:absolute;right:8px;top:34px;background:var(--ws-hover);border:1px solid var(--ws-line);
  border-radius:8px;padding:4px;min-width:112px;z-index:20;box-shadow:0 8px 24px rgba(0,0,0,.45)}
.ws-menu button{display:block;width:100%;text-align:left;border:0;background:transparent;
  color:var(--ws-text);font:inherit;font-size:12px;padding:6px 10px;border-radius:5px;cursor:pointer}
.ws-menu button:hover{background:var(--ws-line)}
.ws-menu hr{border:0;border-top:1px solid var(--ws-line);margin:4px 2px}
.ws-progress{height:6px;border-radius:3px;background:var(--ws-line);overflow:hidden;margin-top:10px}
.ws-progress>i{display:block;height:100%;background:var(--ws-accent);transition:width .2s}
.ws-info{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:12px;margin-top:12px;
  padding-top:12px;border-top:1px solid var(--ws-line)}
.ws-info dt{color:var(--ws-muted);font-size:11px;margin:0 0 3px}
.ws-info dd{margin:0;font-size:12px;color:var(--ws-text);overflow:hidden;text-overflow:ellipsis;
  white-space:nowrap}
.ws-task-foot{display:flex;align-items:center;gap:10px;margin-top:12px;flex-wrap:wrap}
.ws-issues{margin-top:8px;color:var(--ws-danger);font-size:12px}
.ws-empty{padding:48px 10px;text-align:center;color:var(--ws-muted)}
.ws-empty strong{display:block;color:var(--ws-text);font-size:14px;font-weight:500;margin-bottom:6px}
.ws-table{width:100%;border-collapse:collapse;font-size:12px}
.ws-table th{text-align:left;color:var(--ws-muted);font-weight:500;padding:7px 8px;letter-spacing:.02em;
  border-bottom:1px solid var(--ws-line);position:sticky;top:0;background:var(--ws-bg)}
.ws-table td{padding:7px 8px;border-bottom:1px solid var(--ws-line-soft);vertical-align:top}
.ws-table tbody tr:hover{background:var(--ws-input)}
.ws-table tbody tr:nth-child(even){background:var(--ws-zebra)}
.ws-table tbody tr:nth-child(even):hover{background:var(--ws-input)}
.ws-table tbody tr:last-child td{border-bottom:0}
/* 序号列窄、右对齐；路径列吃掉剩余宽度，太长时省略号，鼠标悬停看完整路径。 */
.ws-table th.ws-col-idx,.ws-table td.ws-col-idx{width:40px;text-align:right;color:var(--ws-muted)}
/* 路径要能看全：不省略、允许在分隔符处换行，长路径自然折成两行。 */
.ws-table td.ws-path{white-space:normal;word-break:break-all;min-width:240px;color:var(--ws-text-soft)}
/* 密钥这类敏感字段：输入框 + 右侧「显示/隐藏」，按钮不抢输入框的宽度 */
.ws-field-row{display:flex;align-items:center;gap:8px}
.ws-field-row .ws-input{flex:1 1 auto;min-width:0}
.ws-field-row .ws-btn{flex:0 0 auto;height:32px}
/* 向导第一步的两个入口：加入 / 创建，别再让人猜 */
.ws-tabs{display:flex;gap:6px;margin-bottom:12px}
.ws-tab{flex:1 1 0;height:32px;border:1px solid var(--ws-line);background:var(--ws-card);color:var(--ws-dim);
  border-radius:6px;font:inherit;font-size:12px;cursor:pointer}
.ws-tab[data-active="true"]{background:var(--ws-active);border-color:var(--ws-accent);color:var(--ws-text)}
.ws-mono{font-family:ui-monospace,Consolas,monospace;font-size:11px;color:var(--ws-dim)}
.ws-toolbar{display:flex;flex-wrap:wrap;gap:8px;align-items:center;margin:10px 0 12px}
.ws-input,.ws-select{background:var(--ws-card);border:1px solid var(--ws-line);border-radius:6px;
  color:var(--ws-text);font:inherit;font-size:12px;padding:5px 8px;min-width:0}
.ws-input:focus,.ws-select:focus{outline:none;border-color:var(--ws-accent)}
.ws-field{margin-bottom:10px}
.ws-field label{display:block;color:var(--ws-dim);font-size:12px;margin-bottom:4px}
.ws-field .ws-input,.ws-field .ws-select{width:100%;box-sizing:border-box;padding:6px 9px;height:32px}
.ws-field .ws-num{max-width:180px}
.ws-field .ws-hint{color:var(--ws-muted);font-size:11px;margin-top:4px}
.ws-overlay{position:absolute;inset:0;background:rgba(0,0,0,.55);display:flex;
  align-items:center;justify-content:center;z-index:40}
.ws-modal{background:var(--ws-side);border:1px solid var(--ws-line);border-radius:12px;width:min(620px,92%);
  max-height:88%;display:flex;flex-direction:column;box-shadow:0 24px 60px rgba(0,0,0,.6)}
.ws-modal-head{padding:14px 18px;border-bottom:1px solid var(--ws-line);font-size:15px;font-weight:600;
  flex:0 0 auto}
/* min-height:0 是 flex 子项能真正滚动的关键，否则内容会把弹窗撑破 */
.ws-modal-body{padding:14px 18px;overflow:auto;min-height:0;flex:1 1 auto}
.ws-modal-body::-webkit-scrollbar{width:8px}
.ws-modal-body::-webkit-scrollbar-thumb{background:var(--ws-line);border-radius:4px}
.ws-modal-body::-webkit-scrollbar-thumb:hover{background:#3A3D41}
.ws-modal-body::-webkit-scrollbar-track{background:transparent}
.ws-modal-foot{padding:12px 18px;border-top:1px solid var(--ws-line);display:flex;
  justify-content:flex-end;gap:8px;flex:0 0 auto}
.ws-steps{display:flex;gap:8px;margin-bottom:16px}
.ws-step{flex:1;border-top:2px solid var(--ws-line);padding-top:6px;color:var(--ws-faint);font-size:11px;
  transition:border-color .15s,color .15s}
.ws-step[data-state="done"]{border-color:#1E4FA8;color:var(--ws-muted)}
.ws-step[data-state="on"]{border-color:var(--ws-accent);color:var(--ws-text);font-weight:600}
.ws-toast{position:absolute;left:50%;transform:translateX(-50%);bottom:18px;z-index:60;
  background:var(--ws-hover);border:1px solid var(--ws-line);border-radius:8px;padding:8px 14px;
  font-size:12px;box-shadow:0 8px 24px rgba(0,0,0,.45);max-width:70%}
.ws-toast[data-tone="bad"]{border-color:#5A2A26;color:#FFB4AF}
.ws-toast[data-tone="ok"]{border-color:#25503C;color:#9BE3BF}
.ws-banner{padding:8px 12px;border-radius:8px;font-size:12px;
  background:#2A2318;border:1px solid #4A3A18;color:var(--ws-warn);display:flex;gap:10px;align-items:center}
.ws-raw{position:absolute;right:16px;bottom:16px;width:min(680px,72%);max-height:56%;
  background:var(--ws-side);border:1px solid var(--ws-line);border-radius:10px;display:flex;
  flex-direction:column;z-index:50;box-shadow:0 18px 40px rgba(0,0,0,.55)}
.ws-raw-head{display:flex;align-items:center;justify-content:space-between;padding:8px 12px;
  border-bottom:1px solid var(--ws-line);font-size:12px;color:var(--ws-dim)}
.ws-raw-body{overflow:auto;padding:8px 12px;font-family:ui-monospace,Consolas,monospace;
  font-size:11.5px;line-height:1.65;color:var(--ws-dim);white-space:pre-wrap;word-break:break-all}
.ws-link{color:#5B92FF;cursor:pointer;background:none;border:0;font:inherit;padding:0}
/* 开关：比原生复选框更像「设置项」，也和卡片里的圆钮风格统一 */
.ws-switch{display:inline-flex;align-items:center;gap:8px;cursor:pointer;color:var(--ws-dim);font-size:12px}
.ws-switch input{appearance:none;-webkit-appearance:none;width:32px;height:18px;border-radius:9px;
  background:var(--ws-line);position:relative;margin:0;cursor:pointer;transition:background .15s}
.ws-switch input:checked{background:var(--ws-accent)}
.ws-switch input::after{content:"";position:absolute;top:2px;left:2px;width:14px;height:14px;
  border-radius:50%;background:var(--ws-text);transition:transform .15s}
.ws-switch input:checked::after{transform:translateX(14px)}
.ws-switch:hover{color:var(--ws-text)}

/* 面板宽度取决于 dsh 的分栏，不跟视口走，所以用容器查询而不是媒体查询 */
@container (max-width: 820px){
  .ws-info{grid-template-columns:repeat(2,minmax(0,1fr))}
  .ws-raw{width:min(680px,86%)}
}
@container (max-width: 640px){
  .ws-side{width:132px;flex-basis:132px;padding:12px 8px 10px}
  .ws-brand-name{display:none}
  .ws-info{grid-template-columns:minmax(0,1fr);gap:8px}
  .ws-scroll{padding:14px 14px 24px}
  .ws-raw{width:calc(100% - 24px);right:12px}
}
`

    let stylesInjected = false
    function ensureStyles() {
      if (stylesInjected || typeof document === 'undefined') return
      const tag = document.createElement('style')
      tag.setAttribute('data-winsync', '')
      tag.textContent = CSS
      document.head.appendChild(tag)
      stylesInjected = true
    }

    /* ------------------------------------------------------------ 基础工具 */

    class ApiError extends Error {
      constructor(code, message, body) {
        super(message)
        this.code = code
        this.body = body
      }
    }

    async function api(route, { method = 'GET', body, query } = {}) {
      const search = query ? `?${new URLSearchParams(query).toString()}` : ''
      const response = await fetch(`${API}${route}${search}`, {
        method,
        headers: {
          accept: 'application/json',
          ...(body === undefined ? {} : { 'content-type': 'application/json' }),
        },
        body: body === undefined ? undefined : JSON.stringify(body),
      })
      const text = await response.text()
      let parsed
      try {
        parsed = text === '' ? null : JSON.parse(text)
      } catch {
        parsed = { raw: text }
      }
      if (!response.ok || parsed?.ok === false) {
        const code = parsed?.error?.code ?? `http-${response.status}`
        const message = parsed?.error?.message ?? `HTTP ${response.status}`
        throw new ApiError(code, message, parsed)
      }
      return parsed
    }

    const pad = (n) => String(n).padStart(2, '0')
    function fmtTime(secs) {
      if (!secs) return '—'
      const d = new Date(secs * 1000)
      return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
    }
    function fmtFull(secs) {
      if (!secs) return '—'
      const d = new Date(secs * 1000)
      return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(
        d.getMinutes(),
      )}:${pad(d.getSeconds())}`
    }
    function modeLabel(mode) {
      return mode === 'upload_only' ? '仅上传' : mode === 'download_only' ? '仅下载' : '双向'
    }
    function shortServer(server) {
      return String(server ?? '')
        .replace(/^wss?:\/\//, '')
        .replace(/\/ws$/, '')
    }

    const DEFAULT_SYNC_PORT = 8443

    /**
     * 界面只让用户填 IP 或 IP:端口，协议头 / 路径 / 默认端口都在这里补。
     * 已经写了 ws:// 或 wss:// 的完整地址原样保留——老配置和反代部署不受影响。
     */
    function normalizeServer(raw) {
      const text = String(raw ?? '').trim()
      if (!text) return ''
      const scheme = /^(wss?):\/\//i.exec(text)
      const rest = scheme ? text.slice(scheme[0].length) : text
      const [authority, ...tail] = rest.replace(/^\/+/, '').split('/')
      if (!authority) return ''
      const host = /:\d+$/.test(authority) ? authority : `${authority}:${DEFAULT_SYNC_PORT}`
      const path = tail.length > 0 ? `/${tail.join('/')}` : '/ws'
      return `${scheme ? scheme[1].toLowerCase() : 'ws'}://${host}${path}`
    }

    /**
     * 房间码：32 位随机串，同时也是配对凭证。
     * 中继只按房间名配对、不维护名单，所以「猜不到的房间名」就是门禁——
     * 给别人用的时候不用再去服务器加一条配置。
     */
    function newRoomCode() {
      const buf = new Uint8Array(16)
      if (globalThis.crypto?.getRandomValues) {
        globalThis.crypto.getRandomValues(buf)
      } else {
        for (let i = 0; i < buf.length; i += 1) buf[i] = Math.floor(Math.random() * 256)
      }
      return `k${Array.from(buf, (b) => b.toString(16).padStart(2, '0')).join('')}`
    }

    /**
     * 邀请串 = 中继地址 + 房间名 + 密钥，和 daemon 真正连的那条地址同构：
     * `ws://host:port/ws?workspace=<房间>&token=<密钥>`。
     *
     * 以前用的是 `...#房间码`，只带房间、不带密钥——中继一配房间密钥，
     * 拿邀请串建出来的任务必然 401。现在两者合并，复制邀请 = 复制可用配置。
     */
    function inviteOf(server, room, token) {
      const url = normalizeServer(server)
      const code = String(room ?? '').trim()
      if (!code) return url
      const params = new URLSearchParams()
      params.set('workspace', code)
      params.set('token', String(token ?? '').trim() || code)
      return `${url}?${params.toString()}`
    }

    /**
     * 反过来：把粘进来的文本拆成服务器 / 房间码 / 密钥。
     * 认三种写法：完整邀请串、旧的 `...#房间码`、单独的房间码或 6 位旧配对码。
     */
    function parseInvite(text) {
      const raw = String(text ?? '').trim()
      if (!raw) return null
      const full = /^(wss?:\/\/[^\s#?]+)(?:\?([^\s#]*))?(?:#([^\s]*))?$/i.exec(raw)
      if (full) {
        const query = new URLSearchParams(full[2] ?? '')
        const hashRoom = String(full[3] ?? '').trim()
        const room = String(query.get('workspace') ?? query.get('room') ?? hashRoom ?? '').trim()
        if (!room) return null
        const token = String(query.get('token') ?? '').trim() || room
        return { server: full[1], room, token }
      }
      if (/^\S{4,}$/.test(raw) && !/[\\/]/.test(raw)) {
        const room = /^\d{6}$/.test(raw) ? `pair-${raw}` : raw
        return { server: '', room, token: room }
      }
      return null
    }

    /**
     * dsh 的主题标记：body 上有 `data-ds-dark-theme` 就是深色，没有就是浅色；
     * 再退一步看 html 的 color-scheme，最后才用系统偏好。
     */
    function readTheme() {
      if (typeof document === 'undefined') return 'dark'
      const body = document.body
      if (body && body.hasAttribute('data-ds-dark-theme')) return 'dark'
      const scheme = document.documentElement?.style?.colorScheme || ''
      if (scheme === 'light') return 'light'
      if (scheme === 'dark') return 'dark'
      return window.matchMedia?.('(prefers-color-scheme: light)').matches ? 'light' : 'dark'
    }

    /** 记录页把相对路径拼回磁盘绝对路径（任务目录 + 相对路径），分隔符统一成反斜杠。 */
    function fullPath(root, rel) {
      // 任务目录可能是正斜杠（API 建的任务）也可能是反斜杠（界面填的），先统一成反斜杠。
      let base = String(root ?? '').trim().replace(/\//g, '\\').replace(/\\+$/, '')
      if (base.startsWith('\\\\')) {
        // UNC 路径（双反斜杠开头）要保留开头这两个反斜杠，不能压成一个。
        base = `\\\\${base.slice(2).replace(/\\{2,}/g, '\\')}`
      } else {
        base = base.replace(/\\{2,}/g, '\\')
      }
      const tail = String(rel ?? '').replace(/^[\\/]+/, '').replace(/\//g, '\\')
      if (!base) return tail
      return tail ? `${base}\\${tail}` : base
    }
    function statusOf(task) {
      const rt = task.runtime ?? {}
      // running 与 last_error 会同时存在（上一轮失败、这一轮还在等对端），
      // 这时说「同步中」会让人以为错误已经好了，所以单独标成「重试中」。
      if (rt.running) {
        return rt.last_error
          ? { text: '重试中…', tone: '#E6A23C' }
          : { text: '同步中…', tone: '#E6A23C' }
      }
      if (task.paused) return { text: '已暂停', tone: '#6E7176' }
      if (rt.last_error) return { text: '同步失败', tone: '#F0554D' }
      if (rt.last) return { text: rt.last.up_to_date ? '两端已一致' : '同步完成', tone: '#4CC38A' }
      return { text: '等待同步', tone: '#6E7176' }
    }

    /** 变更统计：全是 0 时不必把四个 0 都念一遍。 */
    function changeSummary(last) {
      if (!last) return null
      const parts = []
      if (last.pushed) parts.push(`推送 ${last.pushed}`)
      if (last.pulled) parts.push(`拉取 ${last.pulled}`)
      if (last.deleted) parts.push(`删除 ${last.deleted}`)
      if (last.conflicts) parts.push(`冲突 ${last.conflicts}`)
      if (parts.length === 0) return last.up_to_date ? '无变更' : '无传输'
      return parts.join(' · ')
    }

    /* ------------------------------------------------------------ store */

    function useStore() {
      const [data, setData] = React.useState(null)
      const [error, setError] = React.useState(null)
      const [connected, setConnected] = React.useState(false)
      const [toast, setToast] = React.useState(null)
      const [rawTask, setRawTask] = React.useState(null)
      // 插件自述（版本 + commit）：只取一次。daemon 起不来也要能显示，
      // 否则「界面没反应」和「装的是旧包」这两件事分不开。
      const [self, setSelf] = React.useState(null)
      const toastTimer = React.useRef(null)

      React.useEffect(() => {
        let alive = true
        api('/version')
          .then((body) => {
            if (alive) setSelf(body)
          })
          .catch(() => {
            /* 拿不到就不显示角标，不影响主功能 */
          })
        return () => {
          alive = false
        }
      }, [])

      const notify = React.useCallback((text, tone = 'ok') => {
        setToast({ text, tone })
        if (toastTimer.current) clearTimeout(toastTimer.current)
        toastTimer.current = setTimeout(() => setToast(null), 3600)
      }, [])

      const refresh = React.useCallback(async () => {
        try {
          const body = await api('/state')
          setData(body)
          setError(null)
        } catch (e) {
          setError(e)
        }
      }, [])

      const patchRuntime = React.useCallback((taskId, fn) => {
        setData((prev) => {
          if (!prev) return prev
          return {
            ...prev,
            tasks: prev.tasks.map((t) =>
              t.id === taskId ? { ...t, runtime: fn(t.runtime ?? {}) } : t,
            ),
          }
        })
      }, [])

      React.useEffect(() => {
        void refresh()
      }, [refresh])

      React.useEffect(() => {
        let closed = false
        let source = null
        let retry = 400
        let retryTimer = null

        const connect = () => {
          if (closed) return
          source = new EventSource(`${API}/events`)
          source.onopen = () => {
            retry = 400
            setConnected(true)
          }
          source.addEventListener('hello', () => setConnected(true))
          source.addEventListener('state', () => void refresh())
          source.addEventListener('log', (event) => {
            const payload = JSON.parse(event.data)
            patchRuntime(payload.task, (rt) => {
              const logs = [...(rt.logs ?? []), payload.line]
              if (logs.length > LOG_RING) logs.splice(0, logs.length - LOG_RING)
              return { ...rt, logs }
            })
          })
          source.addEventListener('progress', (event) => {
            const payload = JSON.parse(event.data)
            patchRuntime(payload.task, (rt) => ({
              ...rt,
              running: true,
              progress: { done: payload.done, total: payload.total, what: payload.what },
            }))
          })
          source.addEventListener('done', (event) => {
            const payload = JSON.parse(event.data)
            patchRuntime(payload.task, (rt) => ({
              ...rt,
              running: false,
              progress: null,
              last: payload.report ?? rt.last ?? null,
              last_error: payload.error ?? null,
              last_time: Math.floor(Date.now() / 1000),
            }))
            void refresh()
          })
          source.addEventListener('lagged', () => void refresh())
          source.onerror = () => {
            setConnected(false)
            source?.close()
            if (closed) return
            retryTimer = setTimeout(connect, retry)
            retry = Math.min(retry * 2, 8000)
          }
        }

        connect()
        const onVisible = () => {
          if (document.visibilityState === 'visible') void refresh()
        }
        document.addEventListener('visibilitychange', onVisible)
        return () => {
          closed = true
          if (retryTimer) clearTimeout(retryTimer)
          document.removeEventListener('visibilitychange', onVisible)
          source?.close()
        }
      }, [patchRuntime, refresh])

      const run = React.useCallback(
        async (work, successText) => {
          try {
            const result = await work()
            if (successText) notify(successText, 'ok')
            await refresh()
            return result
          } catch (e) {
            notify(e?.message ?? String(e), 'bad')
            throw e
          }
        },
        [notify, refresh],
      )

      return {
        data,
        error,
        connected,
        toast,
        notify,
        refresh,
        run,
        rawTask,
        self,
        showRawLog: setRawTask,
        closeRawLog: () => setRawTask(null),
      }
    }

    /* ------------------------------------------------------------ 小图标 */

    const IconFolder = () =>
      h(
        'svg',
        { width: 18, height: 18, viewBox: '0 0 16 16', fill: 'currentColor', 'aria-hidden': 'true' },
        h('path', {
          d: 'M2 4.2c0-.6.5-1 1-1h3l1.2 1.3H13c.6 0 1 .4 1 1v6.3c0 .6-.4 1-1 1H3c-.5 0-1-.4-1-1V4.2Z',
        }),
      )

    const IconPause = () =>
      h(
        'svg',
        { width: 12, height: 12, viewBox: '0 0 12 12', fill: 'currentColor', 'aria-hidden': 'true' },
        h('rect', { x: 2.4, y: 1.8, width: 2.6, height: 8.4, rx: 1 }),
        h('rect', { x: 7, y: 1.8, width: 2.6, height: 8.4, rx: 1 }),
      )

    const IconPlay = () =>
      h(
        'svg',
        { width: 12, height: 12, viewBox: '0 0 12 12', fill: 'currentColor', 'aria-hidden': 'true' },
        h('path', { d: 'M3.6 2.1 9.6 6l-6 3.9V2.1Z' }),
      )

    const IconMore = () =>
      h(
        'svg',
        { width: 14, height: 4, viewBox: '0 0 14 4', fill: 'currentColor', 'aria-hidden': 'true' },
        h('circle', { cx: 2, cy: 2, r: 1.25 }),
        h('circle', { cx: 7, cy: 2, r: 1.25 }),
        h('circle', { cx: 12, cy: 2, r: 1.25 }),
      )

    /* ------------------------------------------------------------ 任务卡 */

    function TaskCard({ task, onEdit, onSync, onPause, onOpenDir, onDelete, onShowLog, onCopyInvite }) {
      const [menu, setMenu] = React.useState(false)
      const status = statusOf(task)
      const rt = task.runtime ?? {}
      const progress = rt.progress
      const frac = progress && progress.total > 0 ? progress.done / progress.total : 0
      const canSync = !rt.running && (task.issues ?? []).length === 0

      return h(
        'div',
        {
          className: 'ws-card',
          'data-selected': menu ? 'true' : 'false',
          style: { position: 'relative' },
        },
        h(
          'div',
          { className: 'ws-card-head' },
          h('div', { className: 'ws-folder' }, h(IconFolder)),
          h(
            'div',
            { style: { minWidth: 0 } },
            h('div', { className: 'ws-task-name' }, task.display_name),
            h('div', { className: 'ws-task-state', style: { color: status.tone } }, status.text),
          ),
          h(
            'div',
            { className: 'ws-card-actions' },
            h(
              'button',
              {
                className: 'ws-icon-btn',
                title: task.paused ? '恢复自动同步' : '暂停自动同步',
                onClick: () => onPause(task),
              },
              task.paused ? h(IconPlay) : h(IconPause),
            ),
            h(
              'button',
              { className: 'ws-icon-btn', title: '更多', onClick: () => setMenu((v) => !v) },
              h(IconMore),
            ),
          ),
        ),
        menu
          ? h(
              'div',
              { className: 'ws-menu' },
              h('button', { onClick: () => { setMenu(false); onEdit(task) } }, '设置'),
              h('button', { onClick: () => { setMenu(false); void onCopyInvite(task) } }, '复制邀请'),
              h('button', { onClick: () => { setMenu(false); onOpenDir(task) } }, '打开目录'),
              h('button', { onClick: () => { setMenu(false); onShowLog(task) } }, '查看日志'),
              h('hr'),
              h('button', { onClick: () => { setMenu(false); onDelete(task) } }, '删除任务'),
            )
          : null,
        progress
          ? h(
              'div',
              null,
              h(
                'div',
                { className: 'ws-progress' },
                h('i', { style: { width: `${Math.round(frac * 100)}%` } }),
              ),
              h(
                'div',
                { className: 'ws-sub', style: { marginTop: 4 } },
                `${progress.done}/${progress.total} · ${progress.what}`,
              ),
            )
          : null,
        h(
          'dl',
          { className: 'ws-info' },
          h('div', null, h('dt', null, '电脑文件路径'), h('dd', { title: task.root }, task.root || '—')),
          h(
            'div',
            null,
            h('dt', null, '目标房间'),
            h('dd', null, `${task.server_short || shortServer(task.server)} / ${task.workspace}`),
          ),
          h(
            'div',
            null,
            h('dt', null, '规则设置'),
            h('dd', null, `${modeLabel(task.mode)} · ${task.filter_summary}`),
          ),
        ),
        (task.issues ?? []).length > 0
          ? h('div', { className: 'ws-issues' }, task.issues.join('；'))
          : null,
        h(
          'div',
          { className: 'ws-task-foot' },
          h(
            'button',
            {
              // 暂停的任务只是在停自动同步，手动同步是次要动作，不要抢主按钮的视觉权重
              className: task.paused ? 'ws-btn' : 'ws-btn ws-btn-primary',
              disabled: !canSync,
              title: rt.running ? '正在同步' : (task.issues ?? []).length > 0 ? '先修正配置问题' : '',
              onClick: () => onSync(task),
            },
            rt.running ? '同步中…' : '立即同步',
          ),
          h(
            'span',
            { className: 'ws-sub' },
            rt.last_time ? `上次同步 ${fmtTime(rt.last_time)}` : '还没同步过',
          ),
          rt.next_auto_in !== undefined && !task.paused
            ? h('span', { className: 'ws-sub' }, `· ${rt.next_auto_in}s 后自动`)
            : null,
          changeSummary(rt.last)
            ? h('span', { className: 'ws-sub' }, `· ${changeSummary(rt.last)}`)
            : null,
        ),
        rt.last_error
          ? h(
              'div',
              { className: 'ws-issues' },
              // 正在同步或已暂停时，这条是「上一轮」的结果；否则状态行已经说了「同步失败」
              rt.running || task.paused ? `上次失败：${rt.last_error}` : rt.last_error,
            )
          : null,
      )
    }

    /* ------------------------------------------------------------ 任务页 */

    function TasksPage({ store, onCreate, onEdit }) {
      const data = store.data
      const tasks = data?.tasks ?? []

      const sync = (task) =>
        store.run(
          () => api(`/tasks/${encodeURIComponent(task.id)}/sync`, { method: 'POST' }),
          '已开始同步',
        )

      const pause = (task) =>
        store.run(
          () =>
            api(`/tasks/${encodeURIComponent(task.id)}/${task.paused ? 'resume' : 'pause'}`, {
              method: 'POST',
            }),
          task.paused ? '已恢复自动同步' : '已暂停自动同步',
        )

      const openDir = (task) =>
        store.run(() => api('/open-dir', { method: 'POST', body: { task: task.id } }))

      const remove = (task) => {
        const ok = window.confirm(
          `删除任务「${task.display_name}」？\n\n配置会被移除，历史记录库 tasks/${task.id}.db 会保留。`,
        )
        if (!ok) return
        void store.run(
          () => api(`/tasks/${encodeURIComponent(task.id)}`, { method: 'DELETE' }),
          '任务已删除',
        )
      }

      const toggleAuto = (on) =>
        store.run(
          () => api('/auto', { method: 'POST', body: { on } }),
          on ? '已开启自动同步' : '已暂停自动同步',
        )

      /**
       * 任务卡上的「复制邀请」：把 server+房间+密钥 一次给全。
       * 密钥不在 /state 里，只能按需向 daemon 要一次；要不到就退回「同房间码」。
       */
      const copyInvite = async (task) => {
        let secret = ''
        if (task.has_token) {
          try {
            const body = await api(`/tasks/${encodeURIComponent(task.id)}/token`)
            secret = body.token ?? ''
          } catch (e) {
            store.notify(e.message, 'bad')
            return
          }
        }
        const text = inviteOf(task.server, task.workspace, secret || task.workspace)
        try {
          await navigator.clipboard.writeText(text)
          store.notify('邀请已复制，发给对方即可', 'ok')
        } catch {
          store.notify(`复制失败，可手动复制：${text}`, 'bad')
        }
      }

      return h(
        'div',
        { className: 'ws-scroll' },
        h(
          'div',
          { className: 'ws-page-head' },
          h('h2', { className: 'ws-title' }, '同步任务'),
          h(
            'div',
            { className: 'ws-row' },
            h(
              'label',
              { className: 'ws-switch', title: '开启后按目录变更与固定间隔自动同步' },
              h('input', {
                type: 'checkbox',
                checked: data?.auto ?? true,
                onChange: (e) => void toggleAuto(e.target.checked),
              }),
              '自动同步',
            ),
            h('button', { className: 'ws-btn ws-btn-primary', onClick: onCreate }, '+ 创建同步任务'),
          ),
        ),
        h(
          'div',
          { className: 'ws-device' },
          // 只报设备与任务数：房间是每个任务各自的，写在这里会被当成全局设置
          tasks.length === 0
            ? `🖥  ${data?.device ?? '本机'}`
            : `🖥  ${data?.device ?? '本机'}  ·  ${tasks.length} 个任务`,
        ),
        tasks.length === 0
          ? h(
              'div',
              { className: 'ws-empty' },
              h('strong', null, '还没有同步任务'),
              '点右上角「+ 创建同步任务」，三步即可开始',
            )
          : tasks.map((task) =>
              h(TaskCard, {
                key: task.id,
                task,
                onEdit,
                onSync: sync,
                onPause: pause,
                onOpenDir: openDir,
                onDelete: remove,
                onShowLog: (t) => store.showRawLog(t.id),
                onCopyInvite: copyInvite,
              }),
            ),
      )
    }

    /* ------------------------------------------------------------ 日志页 */

    function LogsPage({ store }) {
      const tasks = store.data?.tasks ?? []
      const [task, setTask] = React.useState('all')
      const [status, setStatus] = React.useState('all')
      const [q, setQ] = React.useState('')
      const [rows, setRows] = React.useState([])
      const [loading, setLoading] = React.useState(false)
      const notify = store.notify
      // 记录里只有相对路径，展示时拼回磁盘绝对路径；名字重复时取第一个任务即可。
      const roots = React.useMemo(() => {
        const map = new Map()
        for (const item of tasks) {
          if (!map.has(item.display_name)) map.set(item.display_name, item.root)
        }
        return map
      }, [tasks])

      const load = React.useCallback(async () => {
        setLoading(true)
        try {
          const body = await api('/activities', {
            query: { task, status, limit: '500', ...(q.trim() ? { q: q.trim() } : {}) },
          })
          setRows(body.rows ?? [])
        } catch (e) {
          notify(e.message, 'bad')
        } finally {
          setLoading(false)
        }
      }, [notify, q, status, task])

      React.useEffect(() => {
        const timer = setTimeout(() => void load(), 200)
        return () => clearTimeout(timer)
      }, [load])

      const exportCsv = () => {
        const params = new URLSearchParams({ task, status })
        if (q.trim()) params.set('q', q.trim())
        window.open(`${API}/export.csv?${params.toString()}`, '_blank')
      }

      const clear = () => {
        const label =
          task === 'all' ? '全部任务' : tasks.find((t) => t.id === task)?.display_name ?? task
        if (!window.confirm(`清空${label}的同步记录？此操作不可撤销（不影响已同步的文件）。`)) return
        void store
          .run(() => api('/activities', { method: 'DELETE', query: { task } }), '记录已清空')
          .then(load)
      }

      return h(
        'div',
        { className: 'ws-scroll', style: { display: 'flex', flexDirection: 'column' } },
        h('h2', { className: 'ws-title' }, '同步日志'),
        h(
          'div',
          { className: 'ws-toolbar' },
          h(
            'select',
            { className: 'ws-select', value: task, onChange: (e) => setTask(e.target.value) },
            h('option', { value: 'all' }, '全部任务'),
            tasks.map((t) => h('option', { key: t.id, value: t.id }, t.display_name)),
          ),
          h(
            'select',
            { className: 'ws-select', value: status, onChange: (e) => setStatus(e.target.value) },
            h('option', { value: 'all' }, '全部状态'),
            h('option', { value: 'ok' }, '成功'),
            h('option', { value: 'fail' }, '失败'),
          ),
          h('input', {
            className: 'ws-input',
            placeholder: '搜索文件名或说明',
            value: q,
            onChange: (e) => setQ(e.target.value),
            style: { flex: '1 1 180px' },
          }),
          h('button', { className: 'ws-btn', onClick: () => void load() }, loading ? '刷新中…' : '刷新'),
          h('button', { className: 'ws-btn', onClick: exportCsv }, '导出 CSV'),
          h('button', { className: 'ws-btn ws-btn-danger', onClick: clear }, '清空'),
        ),
        rows.length === 0
          ? h(
              'div',
              { className: 'ws-empty' },
              h('strong', null, '暂无记录'),
              '同步过文件之后，这里会逐条列出',
            )
          : h(
              'table',
              { className: 'ws-table' },
              h(
                'thead',
                null,
                h(
                  'tr',
                  null,
                  ['#', '时间', '任务', '文件（完整路径）', '动作', '状态', '说明'].map((label) =>
                    h('th', { key: label, className: label === '#' ? 'ws-col-idx' : undefined }, label),
                  ),
                ),
              ),
              h(
                'tbody',
                null,
                rows.map((row, index) => {
                  const diskPath = fullPath(roots.get(row.task), row.path)
                  return h(
                    'tr',
                    { key: `${row.created_at}-${index}` },
                    h('td', { className: 'ws-mono ws-col-idx' }, String(index + 1)),
                    h('td', { className: 'ws-mono' }, fmtFull(row.created_at)),
                    h('td', null, row.task),
                    h('td', { className: 'ws-mono ws-path', title: diskPath }, diskPath),
                    h('td', null, row.action),
                    h(
                      'td',
                      { style: { color: row.ok ? '#4CC38A' : '#F0554D' } },
                      row.ok ? '成功' : '失败',
                    ),
                    h('td', { title: row.message }, row.message),
                  )
                }),
              ),
            ),
      )
    }

    /* ------------------------------------------------------------ 设置页 */

    function SettingsPage({ store }) {
      const data = store.data
      const settings = data?.settings ?? {}
      const [form, setForm] = React.useState(null)
      const [autostart, setAutostart] = React.useState(null)
      const [diag, setDiag] = React.useState(null)

      React.useEffect(() => {
        setForm({
          limit_up_kbps: settings.limit_up_kbps ?? 0,
          limit_down_kbps: settings.limit_down_kbps ?? 0,
          log_keep_days: settings.log_keep_days ?? 30,
          log_keep_count: settings.log_keep_count ?? 0,
        })
      }, [
        settings.limit_up_kbps,
        settings.limit_down_kbps,
        settings.log_keep_days,
        settings.log_keep_count,
      ])

      React.useEffect(() => {
        api('/autostart')
          .then((body) => setAutostart(body.on))
          .catch(() => setAutostart(null))
      }, [])

      const save = () =>
        store.run(
          () =>
            api('/settings', {
              method: 'PATCH',
              body: {
                limit_up_kbps: Number(form.limit_up_kbps) || 0,
                limit_down_kbps: Number(form.limit_down_kbps) || 0,
                log_keep_days: Number(form.log_keep_days) || 0,
                log_keep_count: Number(form.log_keep_count) || 0,
              },
            }),
          '设置已保存',
        )

      const toggleAutostart = (on) => {
        setAutostart(on)
        void store.run(
          () => api('/autostart', { method: 'POST', body: { on } }),
          on ? '已开启开机自启' : '已关闭开机自启',
        )
      }

      const pruneNow = () =>
        void store.run(() => api('/prune', { method: 'POST' }), '已按保留策略清理')

      const probe = async () => {
        try {
          setDiag(await api('/diag'))
        } catch (e) {
          setDiag({ error: { message: e.message, code: e.code } })
        }
      }

      const field = (label, key, hint) =>
        h(
          'div',
          { className: 'ws-field' },
          h('label', null, label),
          h('input', {
            className: 'ws-input ws-num',
            type: 'number',
            min: 0,
            value: form?.[key] ?? 0,
            onChange: (e) => setForm({ ...form, [key]: e.target.value }),
          }),
          hint ? h('div', { className: 'ws-hint' }, hint) : null,
        )

      return h(
        'div',
        { className: 'ws-scroll' },
        h('h2', { className: 'ws-title' }, '设置'),
        h(
          'div',
          { className: 'ws-sub', style: { margin: '4px 0 3px' } },
          // 两个版本号必须分开：同步服务是 daemon 的，插件是这份 UI/代理的。
          // 以前只写「版本 x」泛指 daemon，升级后界面还是旧的时会被误读成「已经是最新」。
          `同步服务 ${data?.version ?? '—'} · 插件 ${store.self?.label ?? '—'}`,
        ),
        h(
          'div',
          { className: 'ws-sub', style: { margin: '0 0 14px' } },
          `状态目录 ${data?.data_dir ?? '—'}`,
        ),
        h(
          'section',
          { className: 'ws-card' },
          h('div', { style: { fontWeight: 600, marginBottom: 10 } }, '启动与常驻'),
          h(
            'label',
            { className: 'ws-row', style: { gap: 8 } },
            h('input', {
              type: 'checkbox',
              checked: autostart === true,
              disabled: autostart === null,
              onChange: (e) => toggleAutostart(e.target.checked),
            }),
            h('span', null, '开机时自动启动同步服务'),
          ),
          h(
            'div',
            { className: 'ws-sub', style: { marginTop: 6 } },
            '注册表值 WinSyncDaemon，与 exe 的 WinSync 分开；关掉后随 dsh 退出即停。',
          ),
        ),
        h(
          'section',
          { className: 'ws-card' },
          h('div', { style: { fontWeight: 600, marginBottom: 10 } }, '限速（KB/s，0 = 不限）'),
          h(
            'div',
            { style: { display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 } },
            field('上传限速', 'limit_up_kbps'),
            field('下载限速', 'limit_down_kbps'),
          ),
        ),
        h(
          'section',
          { className: 'ws-card' },
          h('div', { style: { fontWeight: 600, marginBottom: 10 } }, '记录保留'),
          h(
            'div',
            { style: { display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 } },
            field('保留天数', 'log_keep_days', '0 = 永久'),
            field('每个任务保留条数', 'log_keep_count', '0 = 不限'),
          ),
          h(
            'div',
            { className: 'ws-row', style: { marginTop: 4 } },
            h('button', { className: 'ws-btn', onClick: pruneNow }, '立即按策略清理'),
          ),
        ),
        h(
          'section',
          { className: 'ws-card', style: { marginTop: 14 } },
          h('div', { style: { fontWeight: 600, marginBottom: 10 } }, '诊断'),
          h(
            'div',
            { className: 'ws-row' },
            h('button', { className: 'ws-btn', onClick: () => void probe() }, '检查同步服务'),
            h('span', { className: 'ws-sub' }, '不改任何数据，只读一次 /health 与握手文件'),
          ),
          diag
            ? h(
                'pre',
                {
                  className: 'ws-mono',
                  style: {
                    marginTop: 10,
                    whiteSpace: 'pre-wrap',
                    wordBreak: 'break-all',
                    color: '#A0A2A6',
                  },
                },
                JSON.stringify(diag, null, 2),
              )
            : null,
        ),
        h(
          'div',
          { className: 'ws-row', style: { justifyContent: 'flex-end', marginTop: 4 } },
          h(
            'button',
            { className: 'ws-btn ws-btn-primary', onClick: () => void save(), disabled: form === null },
            '保存设置',
          ),
        ),
      )
    }

    /* ------------------------------------------------------------ 任务编辑器 */

    function TaskEditor({ task, onClose, store }) {
      const [form, setForm] = React.useState({
        name: task.name,
        // 输入框里只显示 host:port，用户不用面对 ws://…/ws 这种写法
        server: shortServer(task.server),
        token: '',
        workspace: task.workspace,
        root: task.root,
        mode: task.mode,
        dotfiles: task.filter?.ignore_dotfiles ?? false,
        ext: (task.filter?.ignore_extensions ?? []).join(','),
        minMb: task.filter?.size_min ? task.filter.size_min / MB : 0,
        maxMb: task.filter?.size_max ? task.filter.size_max / MB : 0,
      })
      const [busy, setBusy] = React.useState(false)
      const [showToken, setShowToken] = React.useState(false)

      // 密钥不进 /state（那里只有 has_token），打开设置时按需取一次：
      // 「复制邀请」必须带上它，否则对方拿到的是缺密钥的半截配置。
      React.useEffect(() => {
        if (!task.has_token) return
        let alive = true
        void (async () => {
          try {
            const body = await api(`/tasks/${encodeURIComponent(task.id)}/token`)
            if (alive) setForm((prev) => (prev.token ? prev : { ...prev, token: body.token ?? '' }))
          } catch {
            /* 取不到就退回「同房间码」，下面会提示 */
          }
        })()
        return () => {
          alive = false
        }
      }, [task.id, task.has_token])

      const secret = form.token.trim() || form.workspace.trim()

      const copyInvite = async () => {
        const text = inviteOf(form.server, form.workspace, secret)
        try {
          await navigator.clipboard.writeText(text)
          store.notify('邀请已复制，发给对方即可', 'ok')
        } catch {
          store.notify(`复制失败，可手动复制：${text}`, 'bad')
        }
      }

      const revealToken = () => setShowToken((v) => !v)

      const body = (force) => ({
        name: form.name,
        server: normalizeServer(form.server),
        // 中继没配房间密钥时它不校验 token，留空就跟随房间码；配了就必须是那一把
        token: secret,
        workspace: form.workspace,
        root: form.root,
        mode: form.mode,
        filter: {
          ignore_dotfiles: form.dotfiles,
          ignore_extensions: String(form.ext)
            .split(',')
            .map((s) => s.trim().replace(/^\./, ''))
            .filter(Boolean),
          size_min: Number(form.minMb) > 0 ? Math.round(Number(form.minMb) * MB) : null,
          size_max: Number(form.maxMb) > 0 ? Math.round(Number(form.maxMb) * MB) : null,
        },
        ...(force ? { force: true } : {}),
      })

      const submit = async () => {
        const changedRoot = form.root.trim() !== task.root
        if (changedRoot) {
          const ok = window.confirm(
            '更换同步目录会清空本地共识快照，下次同步按全量重建。\n\n继续？',
          )
          if (!ok) return
        }
        setBusy(true)
        try {
          await store.run(
            () =>
              api(`/tasks/${encodeURIComponent(task.id)}`, {
                method: 'PATCH',
                body: body(changedRoot),
              }),
            '已保存',
          )
          onClose()
        } catch {
          /* toast 已经提示过 */
        } finally {
          setBusy(false)
        }
      }

      const input = (label, key, options = {}) =>
        h(
          'div',
          { className: 'ws-field' },
          h('label', null, label),
          h('input', {
            className: 'ws-input',
            value: form[key] ?? '',
            type: options.type ?? 'text',
            placeholder: options.placeholder ?? '',
            onChange: (e) => setForm({ ...form, [key]: e.target.value }),
          }),
          options.hint ? h('div', { className: 'ws-hint' }, options.hint) : null,
          options.preview ? h('div', { className: 'ws-hint ws-mono' }, options.preview) : null,
        )

      return h(
        'div',
        { className: 'ws-overlay', onMouseDown: (e) => e.target === e.currentTarget && onClose() },
        h(
          'div',
          { className: 'ws-modal' },
          h('div', { className: 'ws-modal-head' }, `设置 · ${task.display_name}`),
          h(
            'div',
            { className: 'ws-modal-body' },
            input('任务名称', 'name', { placeholder: '留空则用目录名' }),
            input('服务器地址', 'server', {
              placeholder: '192.168.3.222 或 192.168.3.222:18443',
              hint: `只填 IP 或 IP:端口，不写端口默认 ${DEFAULT_SYNC_PORT}`,
              preview: `将连接 ${normalizeServer(form.server) || '（待填写）'}`,
            }),
            h(
              'div',
              { className: 'ws-field' },
              h('label', null, '房间码'),
              h(
                'div',
                { className: 'ws-field-row' },
                h('input', {
                  className: 'ws-input',
                  value: form.workspace,
                  onChange: (e) => setForm({ ...form, workspace: e.target.value }),
                }),
                h(
                  'button',
                  { type: 'button', className: 'ws-btn', onClick: () => void copyInvite() },
                  '复制邀请',
                ),
              ),
              h(
                'div',
                { className: 'ws-field-row', style: { marginTop: 6 } },
                h(
                  'button',
                  {
                    type: 'button',
                    className: 'ws-btn',
                    onClick: () => setForm({ ...form, workspace: newRoomCode() }),
                  },
                  '换成随机码',
                ),
              ),
              h('div', { className: 'ws-hint' }, '双方必须相同；点「复制邀请」把整串发对方'),
              h('div', { className: 'ws-hint ws-mono', style: { wordBreak: 'break-all' } },
                inviteOf(form.server, form.workspace, secret)),
            ),
            h(
              'div',
              { className: 'ws-field' },
              h('label', null, '密钥'),
              h(
                'div',
                { className: 'ws-field-row' },
                h('input', {
                  className: 'ws-input',
                  type: showToken ? 'text' : 'password',
                  value: form.token,
                  placeholder: '留空则与房间码相同',
                  onChange: (e) => setForm({ ...form, token: e.target.value }),
                }),
                h(
                  'button',
                  { type: 'button', className: 'ws-btn', onClick: revealToken },
                  showToken ? '隐藏' : '显示',
                ),
              ),
              h(
                'div',
                { className: 'ws-hint' },
                '中继启动时配了房间密钥（SYNC_ROOM_TOKENS）才有这一项；两端必须填同一把，留空表示跟随房间码',
              ),
            ),
            h(
              'div',
              { className: 'ws-field' },
              h('label', null, '同步方向'),
              h(
                'select',
                {
                  className: 'ws-select',
                  value: form.mode,
                  onChange: (e) => setForm({ ...form, mode: e.target.value }),
                },
                h('option', { value: 'bidirectional' }, '双向同步'),
                h('option', { value: 'upload_only' }, '仅上传（本机 → 对端）'),
                h('option', { value: 'download_only' }, '仅下载（对端 → 本机）'),
              ),
            ),
            h('div', { style: { fontWeight: 600, margin: '14px 0 8px' } }, '过滤规则'),
            h(
              'label',
              { className: 'ws-row', style: { gap: 8, marginBottom: 10 } },
              h('input', {
                type: 'checkbox',
                checked: form.dotfiles,
                onChange: (e) => setForm({ ...form, dotfiles: e.target.checked }),
              }),
              h('span', null, '忽略点文件（.git、.DS_Store 等）'),
            ),
            input('忽略扩展名', 'ext', { placeholder: '例如 lnk,tmp（逗号分隔）' }),
            h(
              'div',
              { style: { display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 } },
              input('最小大小（MB）', 'minMb', { type: 'number' }),
              input('最大大小（MB）', 'maxMb', { type: 'number' }),
            ),
            h(
              'div',
              { style: { color: '#6E7176', fontSize: 11 } },
              '过滤规则两端必须一致，否则会互相误判删除。',
            ),
          ),
          h(
            'div',
            { className: 'ws-modal-foot' },
            h('button', { className: 'ws-btn ws-btn-ghost', onClick: onClose }, '取消'),
            h(
              'button',
              { className: 'ws-btn ws-btn-primary', disabled: busy, onClick: () => void submit() },
              busy ? '保存中…' : '保存',
            ),
          ),
        ),
      )
    }

    /* ------------------------------------------------------------ 创建向导 */

    function Wizard({ onClose, store }) {
      const [step, setStep] = React.useState(0)
      const [busy, setBusy] = React.useState(false)
      const [entry, setEntry] = React.useState('create')
      // 自检结果：idle / testing / accepted / denied / unreachable / timeout / bad-url / error
      const [probe, setProbe] = React.useState({ state: 'idle', detail: '', at: 0 })
      // 手动「重新测试」的计数器：只用来重跑一次，不改配置
      const [probeNonce, setProbeNonce] = React.useState(0)
      const [form, setForm] = React.useState({
        server: '',
        // 默认就生成一个随机房间码：不用再想名字，也不用去中继登记
        workspace: newRoomCode(),
        // 密钥（中继没配房间密钥时留空即可，等于用房间码）
        token: '',
        joinText: '',
        root: '',
        name: '',
        mode: 'bidirectional',
        dotfiles: false,
        ext: '',
      })

      const room = form.workspace.trim()
      const secret = form.token.trim() || room
      const normalized = normalizeServer(form.server)
      const serverReady = /^wss?:\/\/[^\s/]+/.test(normalized)

      const copyInvite = async () => {
        const text = inviteOf(form.server, room, secret)
        try {
          await navigator.clipboard.writeText(text)
          store.notify('邀请已复制，发给对方即可', 'ok')
        } catch {
          store.notify(`复制失败，可手动复制：${text}`, 'bad')
        }
      }

      /**
       * 连接自检交给 daemon 做，不在浏览器里开 WebSocket。
       *
       * 原因有两个，都是实测出来的：
       *   1. 面板跑在 dsh 的页面里，CSP 与混合内容（https 页面连 ws://）会直接掐掉
       *      到中继的连接，而同一台机器上的 daemon 连得好好的——用户看到的就是
       *      「配对永远连不上」，怎么点都没用；
       *   2. 中继的密钥校验在握手阶段完成（`?workspace=&token=` 不对就 401），
       *      所以「握手成功」就等于「地址、端口、房间码、密钥四样都对」，
       *      而这恰好是用户唯一会填错的部分。
       *
       * 只握手、不发 Hello：占房间位置会让对端 daemon 白等一轮，没必要。
       */
      React.useEffect(() => {
        if (step !== 0 || busy) return undefined
        const url = normalizeServer(form.server)
        const code = form.workspace.trim()
        if (!/^wss?:\/\/[^\s/]+/.test(url) || !code) {
          setProbe({ state: 'idle', detail: '', at: 0 })
          return undefined
        }
        let alive = true
        setProbe({ state: 'testing', detail: '', at: 0 })
        // 房间码是一个字一个字敲的，等停顿再发，别把中继的每分钟配额刷爆
        const timer = setTimeout(async () => {
          try {
            const body = await api('/probe', {
              method: 'POST',
              body: { server: url, workspace: code, token: form.token.trim() || code },
            })
            if (!alive) return
            const result = body?.result ?? {}
            setProbe({ state: result.state ?? 'error', detail: result.message ?? '', at: Date.now() })
          } catch (e) {
            if (alive) setProbe({ state: 'error', detail: e.message, at: Date.now() })
          }
        }, 600)
        return () => {
          alive = false
          clearTimeout(timer)
        }
      }, [step, busy, form.server, form.workspace, form.token, probeNonce])

      const probeTitle = () => {
        if (probe.state === 'testing') return '正在自检…'
        if (probe.state === 'accepted') return '中继已接受这组配置'
        if (probe.state === 'denied') return '中继拒绝了密钥'
        if (probe.state === 'unreachable') return '连不上中继'
        if (probe.state === 'timeout') return '连接超时'
        if (probe.state === 'bad-url') return '服务器地址无效'
        if (probe.state === 'error') return '自检没跑起来'
        return '尚未自检'
      }

      const probeHint = () => {
        if (probe.state === 'accepted') {
          return '地址、房间码、密钥都对。把邀请串发给对方，对方用它建任务后两端就会开始同步；这里不需要等对方在线。'
        }
        if (probe.state === 'denied') {
          return '房间码或密钥与中继配置不一致。中继没配房间密钥时，密钥就等于房间码——所以这里留空即可。'
        }
        if (probe.state === 'unreachable' || probe.state === 'timeout') {
          return `${probe.detail || '检查 IP 与端口'}。端口不是 18443 就写全，例如 192.168.3.222:18443。`
        }
        if (probe.state === 'error') {
          return `同步服务可能还没起来：${probe.detail || '重启 dsh 再试'}`
        }
        return '填好服务器与房间码会自动自检；自检只验证配置，不需要对方同时在线。'
      }

      const issues = []
      if (!room) issues.push('未填写房间码')
      if (!serverReady) issues.push('只填服务器 IP 或 IP:端口即可')
      if (step >= 1) {
        if (!form.root.trim()) issues.push('未选择同步目录')
        else if (!/^([a-zA-Z]:[\\/]|\\\\|\/)/.test(form.root.trim())) {
          issues.push('需要填绝对路径，例如 D:\\Sync')
        }
      }

      // 只有「已经证明这组配置连不上」才拦人：没自检过、或自检通过都放行。
      // 以前要求「对端必须在线」才给下一步，等于逼两台机器同时开着窗口，谁先建谁卡住。
      const blocked =
        probe.state === 'denied' ||
        probe.state === 'unreachable' ||
        probe.state === 'timeout' ||
        probe.state === 'bad-url'

      const create = async () => {
        setBusy(true)
        try {
          await store.run(
            () =>
              api('/tasks', {
                method: 'POST',
                body: {
                  name: form.name,
                  server: normalizeServer(form.server),
                  // 密钥留空就跟随房间码：中继没配房间密钥时它本来就不校验
                  token: secret,
                  workspace: room,
                  root: form.root,
                  mode: form.mode,
                  filter: {
                    ignore_dotfiles: form.dotfiles,
                    ignore_extensions: String(form.ext)
                      .split(',')
                      .map((s) => s.trim().replace(/^\./, ''))
                      .filter(Boolean),
                    size_min: null,
                    size_max: null,
                  },
                },
              }),
            '任务已创建',
          )
          onClose()
        } catch {
          /* toast 已提示 */
        } finally {
          setBusy(false)
        }
      }

      const stepBar = h(
        'div',
        { className: 'ws-steps' },
        ['1 连接中继', '2 本机目录', '3 确认'].map((label, index) =>
          h(
            'div',
            { key: label, className: 'ws-step', 'data-state': index < step ? 'done' : index === step ? 'on' : 'todo' },
            label,
          ),
        ),
      )

      const serverField = h(
        'div',
        { className: 'ws-field' },
        h('label', null, '中继地址'),
        h('input', {
          className: 'ws-input',
          value: form.server,
          placeholder: '192.168.3.222 或 192.168.3.222:18443',
          onChange: (e) => setForm({ ...form, server: e.target.value }),
        }),
        h(
          'div',
          { className: 'ws-hint' },
          `只填 IP 或 IP:端口，不写端口默认 ${DEFAULT_SYNC_PORT}；协议头与 /ws 会自动补上`,
        ),
        h('div', { className: 'ws-hint ws-mono' }, `将连接 ${normalized || '（待填写）'}`),
      )

      const roomField = h(
        'div',
        { className: 'ws-field' },
        h('label', null, '房间码'),
        h(
          'div',
          { className: 'ws-field-row' },
          h('input', {
            className: 'ws-input',
            value: form.workspace,
            placeholder: '两端必须一致',
            onChange: (e) => setForm({ ...form, workspace: e.target.value }),
          }),
          h(
            'button',
            { type: 'button', className: 'ws-btn', onClick: () => setForm({ ...form, workspace: newRoomCode() }) },
            '换一个',
          ),
        ),
        h(
          'div',
          { className: 'ws-hint' },
          '随机码就是房间，也是默认密钥；不用去中继登记，也不用起名字',
        ),
      )

      const tokenField = h(
        'div',
        { className: 'ws-field' },
        h('label', null, '密钥（可选）'),
        h('input', {
          className: 'ws-input',
          value: form.token,
          placeholder: '留空则与房间码相同',
          onChange: (e) => setForm({ ...form, token: e.target.value }),
        }),
        h(
          'div',
          { className: 'ws-hint' },
          '只有中继启动时配了房间密钥（SYNC_ROOM_TOKENS）才需要填；两端必须填同一把',
        ),
      )

      const probeCard = h(
        'div',
        { className: 'ws-card', style: { marginTop: 12 } },
        h(
          'div',
          { className: 'ws-row', style: { justifyContent: 'space-between', alignItems: 'flex-start' } },
          h(
            'div',
            { style: { minWidth: 0, flex: 1 } },
            h(
              'div',
              {
                className: 'ws-pair-title',
                'data-presence': probe.state === 'accepted' ? 'ok' : blocked ? 'bad' : 'idle',
              },
              probe.state === 'accepted' ? `✓ ${probeTitle()}` : blocked ? `✗ ${probeTitle()}` : probeTitle(),
            ),
            h('div', { className: 'ws-hint' }, probeHint()),
          ),
          h(
            'button',
            {
              type: 'button',
              className: 'ws-btn',
              // 卡片行里空间紧，「重新自检」会被挤成两行
              style: { whiteSpace: 'nowrap', flexShrink: 0 },
              disabled: probe.state === 'testing',
              onClick: () => setProbeNonce((n) => n + 1),
            },
            probe.state === 'testing' ? '自检中…' : '重新自检',
          ),
        ),
      )

      const inviteCard = h(
        'div',
        { className: 'ws-field' },
        h('label', null, '邀请串'),
        h('div', { className: 'ws-hint ws-mono', style: { wordBreak: 'break-all', marginBottom: 6 } },
          form.server.trim() ? inviteOf(form.server, room, secret) : '（填写中继地址后生成）'),
        h(
          'button',
          { type: 'button', className: 'ws-btn', disabled: !form.server.trim() || !room, onClick: () => void copyInvite() },
          '复制邀请',
        ),
        h('div', { className: 'ws-hint' }, '整串发给对方，对方粘进「加入已有同步」就能带着地址、房间码、密钥一起填好'),
      )

      const body = () => {
        if (step === 0) {
          return h(
            'div',
            null,
            h(
              'div',
              { className: 'ws-tabs' },
              h(
                'button',
                {
                  type: 'button',
                  className: 'ws-tab',
                  'data-active': entry === 'join' ? 'true' : 'false',
                  onClick: () => setEntry('join'),
                },
                '加入已有同步',
              ),
              h(
                'button',
                {
                  type: 'button',
                  className: 'ws-tab',
                  'data-active': entry === 'create' ? 'true' : 'false',
                  onClick: () => setEntry('create'),
                },
                '创建新同步',
              ),
            ),
            entry === 'join'
              ? h(
                  'div',
                  null,
                  h(
                    'div',
                    { className: 'ws-field' },
                    h('label', null, '邀请串'),
                    h('input', {
                      className: 'ws-input',
                      placeholder: 'ws://192.168.3.222:18443/ws?workspace=…&token=…',
                      value: form.joinText,
                      onChange: (e) => {
                        const text = e.target.value
                        const inv = parseInvite(text)
                        if (inv) {
                          setForm({
                            ...form,
                            joinText: text,
                            // 界面里统一显示 host:port，不把 ws://…/ws 这种写法摊给用户
                            server: inv.server ? shortServer(inv.server) : form.server,
                            workspace: inv.room,
                            token: inv.token && inv.token !== inv.room ? inv.token : '',
                          })
                        } else {
                          setForm({ ...form, joinText: text })
                        }
                      },
                    }),
                    h(
                      'div',
                      { className: 'ws-hint' },
                      '粘贴整串即可带出下面三项；只有 6 位数字也会当成旧版配对码（房间 pair-xxxxxx）',
                    ),
                  ),
                  serverField,
                  roomField,
                  tokenField,
                )
              : h('div', null, serverField, roomField, tokenField, inviteCard),
            probeCard,
          )
        }
        if (step === 1) {
          return h(
            'div',
            null,
            h(
              'div',
              { className: 'ws-field' },
              h('label', null, '本机同步目录'),
              h('input', {
                className: 'ws-input',
                value: form.root,
                placeholder: 'D:\\Sync',
                onChange: (e) => setForm({ ...form, root: e.target.value }),
              }),
              h(
                'div',
                { className: 'ws-hint' },
                '绝对路径即可，目录不存在会自动创建。浏览器拿不到系统目录选择框，请从资源管理器复制路径粘贴。',
              ),
            ),
            h(
              'div',
              { className: 'ws-field' },
              h('label', null, '任务名称（可选）'),
              h('input', {
                className: 'ws-input',
                value: form.name,
                placeholder: '留空则用目录名',
                onChange: (e) => setForm({ ...form, name: e.target.value }),
              }),
            ),
          )
        }
        return h(
          'div',
          null,
          h(
            'div',
            { className: 'ws-field' },
            h('label', null, '同步方向'),
            h(
              'select',
              {
                className: 'ws-select',
                value: form.mode,
                onChange: (e) => setForm({ ...form, mode: e.target.value }),
              },
              h('option', { value: 'bidirectional' }, '双向同步'),
              h('option', { value: 'upload_only' }, '仅上传（本机 → 对端）'),
              h('option', { value: 'download_only' }, '仅下载（对端 → 本机）'),
            ),
          ),
          h(
            'div',
            { className: 'ws-field' },
            h('label', null, '忽略扩展名（可选）'),
            h('input', {
              className: 'ws-input',
              value: form.ext,
              placeholder: 'lnk,tmp',
              onChange: (e) => setForm({ ...form, ext: e.target.value }),
            }),
          ),
          h(
            'div',
            { className: 'ws-card' },
            h('div', { className: 'ws-sub', style: { marginBottom: 6 } }, '即将创建'),
            h(
              'div',
              { style: { marginBottom: 4 } },
              h(
                'span',
                { className: 'ws-mono', style: { wordBreak: 'break-all' } },
                form.root || '（未填目录）',
              ),
            ),
            h(
              'div',
              { className: 'ws-sub' },
              `⇄  ${shortServer(normalized)}  ·  房间 ${room}  ·  密钥 ${form.token.trim() ? '自定义' : '同房间码'}`,
            ),
            h(
              'div',
              { className: 'ws-sub', style: { marginTop: 4 } },
              probe.state === 'accepted' ? '中继自检通过' : '未自检（不影响创建，同步失败时日志会说明原因）',
            ),
            issues.length > 0
              ? h(
                  'div',
                  { className: 'ws-issues' },
                  issues.map((item) => h('div', { key: item }, `· ${item}`)),
                )
              : h(
                  'div',
                  { style: { color: 'var(--ws-ok)', fontSize: 12, marginTop: 6 } },
                  '· 配置检查通过',
                ),
          ),
        )
      }

      return h(
        'div',
        { className: 'ws-overlay' },
        h(
          'div',
          { className: 'ws-modal' },
          h('div', { className: 'ws-modal-head' }, '创建同步任务'),
          h('div', { className: 'ws-modal-body' }, stepBar, body()),
          h(
            'div',
            { className: 'ws-modal-foot' },
            h('button', { className: 'ws-btn ws-btn-ghost', onClick: onClose }, '取消'),
            step > 0
              ? h('button', { className: 'ws-btn', onClick: () => setStep(step - 1) }, '上一步')
              : null,
            step < 2
              ? h(
                  'button',
                  {
                    className: 'ws-btn ws-btn-primary',
                    disabled: issues.length > 0 || blocked,
                    title:
                      issues.length > 0
                        ? issues.join('；')
                        : blocked
                          ? probeHint()
                          : '',
                    onClick: () => setStep(step + 1),
                  },
                  '下一步',
                )
              : h(
                  'button',
                  {
                    className: 'ws-btn ws-btn-primary',
                    disabled: busy || issues.length > 0,
                    onClick: () => void create(),
                  },
                  busy ? '创建中…' : '创建',
                ),
          ),
        ),
      )
    }

    /* ------------------------------------------------------------ 原始日志浮层 */

    function RawLog({ task, lines, onClose }) {
      const box = React.useRef(null)
      React.useEffect(() => {
        if (box.current) box.current.scrollTop = box.current.scrollHeight
      }, [lines])
      return h(
        'div',
        { className: 'ws-raw' },
        h(
          'div',
          { className: 'ws-raw-head' },
          h('span', null, `同步日志 · ${task?.display_name ?? ''}（最近 ${LOG_RING} 行）`),
          h('button', { className: 'ws-btn ws-btn-ghost', onClick: onClose }, '关闭'),
        ),
        h(
          'div',
          { ref: box, className: 'ws-raw-body' },
          lines.length === 0 ? '（暂无输出）' : lines.join('\n'),
        ),
      )
    }

    /* ------------------------------------------------------------ 版本角标 */

    /**
     * 侧栏底部常驻一行「v0.5.3 · c76a83c」。
     *
     * 排查时最费时间的一类问题是「我明明装了新版，界面还是旧的」——bundle 由页面加载时注入、
     * profile 里是硬链快照、仓库工作区又是另一份。让界面自己报版本，就不用猜了。
     * 点一下复制完整信息（版本 / 提交 / 分支 / 打包时间 / 目录），贴到 issue 里够用。
     */
    function VersionBadge({ store }) {
      const self = store.self
      if (!self) return null

      const short = `v${self.label ?? self.version}`
      const detail = [
        `dsh-winsync ${self.version}`,
        `提交    ${self.commit ?? '未知'}${self.commitSource ? `（来源：${self.commitSource}）` : ''}`,
        self.branch ? `分支    ${self.branch}` : null,
        self.builtAt ? `打包    ${self.builtAt}` : null,
        self.dirty ? '工作区  有未提交改动' : null,
        `目录    ${self.dir ?? '—'}`,
      ]
        .filter(Boolean)
        .join('\n')

      const copy = async () => {
        try {
          await navigator.clipboard.writeText(detail)
          store.notify('版本信息已复制', 'ok')
        } catch {
          store.notify(`复制失败：${self.version} / ${self.commit ?? '未知提交'}`, 'bad')
        }
      }

      return h(
        'button',
        {
          className: 'ws-ver',
          'data-dirty': self.dirty ? 'true' : 'false',
          title: `${detail}\n\n点击复制`,
          'aria-label': `插件 ${short}，点击复制版本信息`,
          onClick: () => void copy(),
        },
        short,
      )
    }

    /* ------------------------------------------------------------ App */

    function WinsyncApp() {
      ensureStyles()
      const store = useStore()
      const [theme, setTheme] = React.useState(readTheme)
      React.useEffect(() => {
        const sync = () => setTheme(readTheme())
        const observer = new MutationObserver(sync)
        if (document.body) observer.observe(document.body, { attributes: true, attributeFilter: ['data-ds-dark-theme'] })
        observer.observe(document.documentElement, { attributes: true, attributeFilter: ['style', 'data-ds-theme-source'] })
        const media = window.matchMedia?.('(prefers-color-scheme: light)')
        media?.addEventListener?.('change', sync)
        sync()
        return () => {
          observer.disconnect()
          media?.removeEventListener?.('change', sync)
        }
      }, [])
      const [tab, setTab] = React.useState('tasks')
      const [editing, setEditing] = React.useState(null)
      const [wizard, setWizard] = React.useState(false)

      const data = store.data
      const rawLogTask = store.rawTask ? data?.tasks.find((t) => t.id === store.rawTask) ?? null : null

      const nav = (id, label) =>
        h(
          'button',
          {
            className: 'ws-nav-item',
            'data-active': tab === id ? 'true' : 'false',
            onClick: () => setTab(id),
          },
          label,
        )

      return h(
        'div',
        { className: 'ws-root', 'data-theme': theme },
        h(
          'aside',
          { className: 'ws-side' },
          h(
            'div',
            { className: 'ws-brand' },
            h('div', { className: 'ws-mark' }, h(WinsyncIcon, { size: 18 })),
            h('div', { className: 'ws-brand-name' }, 'WinSync'),
          ),
          h('nav', { className: 'ws-nav' }, nav('tasks', '同步任务'), nav('logs', '同步日志'), nav('settings', '设置')),
          h(
            'div',
            { className: 'ws-side-foot' },
            h('button', { className: 'ws-nav-item', onClick: () => setWizard(true) }, '+ 创建同步任务'),
            h(VersionBadge, { store }),
          ),
        ),
        h(
          'main',
          { className: 'ws-content', style: { position: 'relative' } },
          store.connected
            ? null
            : h(
                'div',
                { className: 'ws-banner', style: { margin: '12px 24px 0' } },
                h('span', null, '实时连接中断，正在重连…'),
                h('button', { className: 'ws-link', onClick: () => void store.refresh() }, '手动刷新'),
              ),
          store.error
            ? h(
                'div',
                {
                  className: 'ws-banner',
                  style: {
                    margin: '12px 24px 0',
                    background: '#2A1715',
                    borderColor: '#5A2A26',
                    color: '#FFB4AF',
                  },
                },
                h('span', null, `同步服务不可用：${store.error.message}`),
                h('button', { className: 'ws-link', onClick: () => void store.refresh() }, '重试'),
              )
            : null,
          !data && !store.error
            ? h('div', { className: 'ws-scroll' }, h('div', { className: 'ws-empty' }, '正在读取同步状态…'))
            : tab === 'tasks'
              ? h(TasksPage, { store, onCreate: () => setWizard(true), onEdit: setEditing })
              : tab === 'logs'
                ? h(LogsPage, { store })
                : h(SettingsPage, { store }),
          rawLogTask
            ? h(RawLog, {
                task: rawLogTask,
                lines: rawLogTask.runtime?.logs ?? [],
                onClose: store.closeRawLog,
              })
            : null,
          store.toast
            ? h('div', { className: 'ws-toast', 'data-tone': store.toast.tone }, store.toast.text)
            : null,
        ),
        wizard ? h(Wizard, { onClose: () => setWizard(false), store }) : null,
        editing ? h(TaskEditor, { task: editing, onClose: () => setEditing(null), store }) : null,
      )
    }

    /* ------------------------------------------------------------ 注册 */

    function WinsyncIcon({ size }) {
      const edge = typeof size === 'number' && size > 0 ? size : 18
      return h(
        'svg',
        {
          width: edge,
          height: edge,
          viewBox: '0 0 16 16',
          fill: 'none',
          stroke: 'currentColor',
          strokeWidth: 1.5,
          strokeLinecap: 'round',
          strokeLinejoin: 'round',
          'aria-hidden': 'true',
        },
        h('path', { d: 'M2.8 6.6a5.1 5.1 0 0 1 9.5-1.9' }),
        h('path', { d: 'M13.2 2.4v2.7H10.5' }),
        h('path', { d: 'M13.2 9.4a5.1 5.1 0 0 1-9.5 1.9' }),
        h('path', { d: 'M2.8 13.6v-2.7h2.7' }),
      )
    }

    return {
      name: 'dsh-winsync',
      inject: ['slots'],
      apply(ctx) {
        ctx.slots.inject('main', () =>
          ctx.slots.register({ name: 'main', key: PANEL_ID }, WinsyncApp),
        )
        ctx.slots.inject('sidebar.panellist', () =>
          ctx.slots.register(
            // 菜单名说清楚「这是文件同步」，别和 dsh 自己的「同步」概念混淆。
            { name: 'sidebar.panellist', id: PANEL_ID, order: 10, label: () => '文件同步' },
            WinsyncIcon,
          ),
        )
      },
    }
  },
})
