/**
 * 向导界面截图：不需要 dsh，也不需要真 daemon。
 *
 * 做法：用 jsdom 把真 bundle 渲染出来 → 把整页 HTML 连同注入的 <style> 落盘
 * → 只改 `.ws-root` 上的 data-theme 得到深/浅两份 → 用无头 Edge 截图。
 *
 * 之所以能这样换肤：主题全靠 `.ws-root[data-theme]` 上的 CSS 变量，
 * 标记不变、只改变量，所以两份 HTML 的差异就是配色本身。
 *
 *   node test/preview-shots.mjs
 * 产出 test/shots/wiz-0.8-*.png
 */

import { spawn } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

import { JSDOM } from 'jsdom'

const here = path.dirname(fileURLToPath(import.meta.url))
const shots = path.resolve(here, 'shots')
const work = path.resolve(here, '.preview')
mkdirSync(shots, { recursive: true })
mkdirSync(work, { recursive: true })

const BUNDLE = process.env.WINSYNC_CLIENT ?? fileURLToPath(new URL('../lib/client.js', import.meta.url))
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
// jsdom 会把 Event / WebSocket 换成自己的实现，dom 操作完之后要给 CDP 还回去
const REAL_EVENT = globalThis.Event
const REAL_WS = globalThis.WebSocket
globalThis.IS_REACT_ACT_ENVIRONMENT = true

/* ------------------------------------------------------------ 假 daemon */

let presence = { state: 'waiting', peer: '', error: '', room: '' }
const state = {
  ok: true,
  version: '0.8.0',
  device: 'GRAY_GUO',
  auto: true,
  data_dir: 'C:\\Users\\gjn86\\AppData\\Local\\dsh-winsync',
  settings: { limit_up_kbps: 0, limit_down_kbps: 0, log_keep_days: 30, log_keep_count: 0 },
  tasks: [
    {
      id: 't1',
      name: 'docs',
      display_name: '设计稿',
      server: 'ws://127.0.0.1:18444/ws',
      server_short: '127.0.0.1:18444',
      workspace: `k${'7f3a9c2e5b8d1f4a6c0e9b2d5f8a1c3e'}`,
      root: 'D:\\Sync\\design',
      mode: 'bidirectional',
      filter_summary: '忽略 .git / lnk',
      paused: false,
      has_token: true,
      issues: [],
      runtime: { running: true, progress: { done: 12, total: 40, what: 'design.fig' } },
    },
  ],
}

const dom = new JSDOM('<!doctype html><html><body><div id="app"></div></body></html>', {
  url: 'http://localhost/',
  pretendToBeVisual: true,
})
const { window } = dom
global.window = window
global.document = window.document
Object.defineProperty(global, 'navigator', { value: window.navigator, configurable: true })
for (const key of ['Event', 'MouseEvent', 'InputEvent', 'Element', 'HTMLElement', 'Node', 'MutationObserver']) {
  global[key] = window[key]
}
window.HTMLElement.prototype.focus = () => {}
Object.defineProperty(window.navigator, 'clipboard', {
  value: { writeText: async () => {} },
  configurable: true,
})
class EventSourceStub {
  addEventListener() {}
  close() {}
}
global.EventSource = EventSourceStub
window.EventSource = EventSourceStub

// 面板的 fetch 要被替换成假 daemon，CDP 那条通道得留着真 fetch
const realFetch = globalThis.fetch
global.fetch = async (url, init = {}) => {
  const target = String(url)
  const method = String(init.method ?? 'GET').toUpperCase()
  let body = { ok: true }
  if (target.includes('/presence/stop')) body = { ok: true }
  else if (target.includes('/presence')) {
    if (method === 'POST') {
      const sent = JSON.parse(init.body ?? '{}')
      presence = { ...presence, room: sent.workspace ?? '' }
    }
    body = { ok: true, presence }
  } else if (target.includes('/probe')) {
    body = { ok: true, result: { state: 'unreachable', message: '连不上 ws://10.90.45.64:38443/ws：IO error: 由于连接方在一段时间后没有正确答复或连接主机没有反应，连接尝试失败。 (os error 10060)', elapsed_ms: 6003 } }
  } else if (target.includes('/state')) body = state
  else if (target.includes('/version')) body = { ok: true, version: '0.8.0', label: '0.8.0', dir: 'C:\\repo' }
  else if (target.includes('/activities')) body = { ok: true, rows: [] }
  return { ok: true, status: 200, text: async () => JSON.stringify(body) }
}
window.fetch = global.fetch

/* ------------------------------------------------------------ 渲染 */

const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')

let captured
window.__ModuleLoader__ = { load: (options) => (captured = options) }
new Function('window', readFileSync(BUNDLE, 'utf8'))(window)
const plugin = captured.factory(() => React)
const registered = []
plugin.apply({
  slots: {
    inject: (_key, callback) => callback(),
    register: (options, component) => {
      registered.push({ options, component })
      return () => {}
    },
  },
})
const Page = registered.find((entry) => entry.options.name === 'main').component
const root = createRoot(document.getElementById('app'))
await React.act(async () => {
  root.render(React.createElement(Page, {}))
  await wait(40)
})

const text = () => document.body.textContent ?? ''
const byText = (selector, needle) =>
  [...document.querySelectorAll(selector)].find((node) => (node.textContent ?? '').includes(needle))

async function click(node) {
  if (!node) throw new Error('要点击的元素不存在')
  await React.act(async () => {
    node.dispatchEvent(new window.MouseEvent('click', { bubbles: true }))
    await wait(30)
  })
}

async function typeInto(node, value) {
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set
  await React.act(async () => {
    setter.call(node, value)
    node.dispatchEvent(new window.Event('input', { bubbles: true }))
    await wait(30)
  })
}

/* 1. 任务页（有任务、同步中） */
const taskShot = document.documentElement.outerHTML

/* 2. 向导第一步：已配对 + 高级配置展开显示 */
await click(byText('.ws-nav-item', '创建同步任务'))
assertExists(document.querySelector('.ws-modal'), '向导没打开')
await typeInto(document.querySelector('.ws-modal input[placeholder^="192.168"]'), '10.90.45.64:38443')
await wait(120)
await click(byText('.ws-btn', '测试地址'))
await wait(150)
const channel = document.querySelector('input.ws-channel').value
presence = {
  state: 'paired',
  peer: `ui-7f2a4c91e0~${Buffer.from(`k${'ab'.repeat(16)}|k${'ab'.repeat(16)}`, 'utf8').toString('hex')}`,
  error: '',
  room: channel,
}
await click(byText('.ws-btn', '重新配对'))
// 等防抖 + 一次轮询：这一刻应当是「配对已通过」、通道码置灰
await wait(900)
const wizardCollapsed = document.documentElement.outerHTML

await click(byText('.ws-link', '高级配置'))
await wait(80)
const wizardOpen = document.documentElement.outerHTML

function assertExists(node, message) {
  if (!node) throw new Error(message)
}

/* ------------------------------------------------------------ 落盘 */

function variant(html, theme, dark) {
  const withTheme = html.replace('class="ws-root" data-theme="dark"', `class="ws-root" data-theme="${theme}"`)
  return dark
    ? withTheme.replace('<body>', '<body data-ds-dark-theme="true">')
    : withTheme
}

const pages = {
  'wiz-0.8-tasks-light': variant(taskShot, 'light', false),
  'wiz-0.8-tasks-dark': variant(taskShot, 'dark', true),
  'wiz-0.8-pair-light': variant(wizardCollapsed, 'light', false),
  'wiz-0.8-pair-dark': variant(wizardCollapsed, 'dark', true),
  'wiz-0.8-adv-light': variant(wizardOpen, 'light', false),
  'wiz-0.8-adv-dark': variant(wizardOpen, 'dark', true),
}

const files = {}
for (const [name, html] of Object.entries(pages)) {
  const file = path.join(work, `${name}.html`)
  writeFileSync(file, html, 'utf8')
  files[name] = file
}

/* ------------------------------------------------------------ 截图 */

const BROWSERS = [
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Google/Chrome/Application/chrome.exe',
]
const browser = BROWSERS.find((candidate) => existsSync(candidate))
if (!browser) {
  console.log('没找到 Edge/Chrome，HTML 已落在 test/.preview/，手动打开即可')
  process.exit(0)
}

const DEBUG_PORT = Number(process.env.CDP_PORT ?? 9361)
// 浏览器 profile 放系统临时目录：带点的目录（.preview）下 Edge 起不来
const profileDir = path.join(os.tmpdir(), `ws-preview-${process.pid}`)

const child = spawn(
  browser,
  [
    '--headless=new',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    `--remote-debugging-port=${DEBUG_PORT}`,
    `--user-data-dir=${profileDir}`,
    '--window-size=1180,900',
    'about:blank',
  ],
  // detached：不这么起，Edge 会跟着父进程的作业对象一起被收掉，调试端口永远不响应
  { stdio: 'ignore', windowsHide: true, detached: true },
)
process.on('exit', () => {
  try {
    child.kill()
  } catch {
    /* already gone */
  }
})

async function target() {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    try {
      const list = await (await realFetch(`http://127.0.0.1:${DEBUG_PORT}/json/list`)).json()
      const page = list.find((entry) => entry.type === 'page')
      if (page?.webSocketDebuggerUrl) return page
    } catch {
      /* still starting */
    }
    await wait(200)
  }
  throw new Error('CDP target never appeared')
}

const debugTarget = await target()
globalThis.Event = REAL_EVENT
globalThis.WebSocket = REAL_WS
const socket = new WebSocket(debugTarget.webSocketDebuggerUrl)
await new Promise((resolve, reject) => {
  socket.addEventListener('open', resolve, { once: true })
  socket.addEventListener('error', reject, { once: true })
})

let nextId = 1
const pending = new Map()
socket.addEventListener('message', (event) => {
  const message = JSON.parse(event.data)
  if (message.id === undefined) return
  const entry = pending.get(message.id)
  if (!entry) return
  pending.delete(message.id)
  message.error ? entry.reject(new Error(JSON.stringify(message.error))) : entry.resolve(message.result)
})
const send = (method, params = {}) => {
  const id = nextId++
  socket.send(JSON.stringify({ id, method, params }))
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }))
}

await send('Page.enable')
for (const [name, file] of Object.entries(files)) {
  await send('Page.navigate', { url: `file:///${file.replace(/\\/g, '/')}` })
  await wait(700)
  if (name.includes('adv')) {
    await send('Runtime.evaluate', { expression: "document.querySelector('.ws-modal-body').scrollTop = 9999" })
    await wait(200)
  }
  const png = await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true })
  const out = path.join(shots, `${name}.png`)
  writeFileSync(out, Buffer.from(png.data, 'base64'))
  console.log('shot', out)
}

socket.close()
child.kill()
console.log('preview-shots: 完成')
// 无头浏览器是 detached 的，WS 也还开着：不显式退出，事件循环会一直挂着
process.exit(0)
