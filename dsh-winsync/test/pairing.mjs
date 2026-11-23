/**
 * 配对交互测试：自检交给 daemon，浏览器不开 WebSocket。
 *
 * 旧的实现（0.5.x）是「进向导就在浏览器里连中继、挂在 `<房间>::pair` 旁路房间里等对端」，
 * 实测有两个走不通的地方：
 *   1. 面板跑在 dsh 页面里，CSP / 混合内容会掐掉到 ws:// 中继的连接，
 *      而同一台机器上的 daemon 连得好好的 → 用户看到「配对永远连不上」；
 *   2. 它要求两端同时待在向导第一步，谁先建好任务谁就「不在等待房间」，
 *      于是另一端永远等不到人。
 *
 * 新语义：进第一步就调 daemon 的 POST /api/probe，只做一次 WebSocket 握手
 * （中继的密钥校验就在握手阶段），所以
 *   - 握手成功 = 地址、端口、房间码、密钥四样都对；
 *   - 不发 Hello、不占房间，不会把对端 daemon 当成同步对端；
 *   - 结果只用于「别让用户带着错配置往下走」，不再拿「对端在线」当门禁。
 *
 * 用法（需要 react + react-dom + jsdom）：
 *   ln -s <有这些依赖的目录> node_modules && node test/pairing.mjs
 */

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { JSDOM } from 'jsdom'

globalThis.IS_REACT_ACT_ENVIRONMENT = true

// 用 fileURLToPath：Windows 上 URL.pathname 会给出 /C:/…，readFileSync 拼成 C:\C:\…
const BUNDLE = process.env.WINSYNC_CLIENT ?? fileURLToPath(new URL('../lib/client.js', import.meta.url))

/* ------------------------------------------------------------ 假 daemon */

const probes = [] // 每次 /api/probe 的请求体
let probeReply = { state: 'accepted', message: '中继已接受：地址、房间码与密钥都对', elapsed_ms: 12 }

/** 浏览器里一次 WebSocket 都不该建：配对的网络动作全部在 daemon 侧。 */
const sockets = []
class ForbiddenSocket {
  constructor(url) {
    sockets.push(url)
    throw new Error(`不应在浏览器里连中继：${url}`)
  }
}

/* ------------------------------------------------------------ 渲染环境 */

function setup() {
  const dom = new JSDOM('<!doctype html><html><body><div id="app"></div></body></html>', {
    url: 'http://localhost/',
    pretendToBeVisual: true,
  })
  const { window } = dom
  global.window = window
  global.document = window.document
  Object.defineProperty(global, 'navigator', { value: window.navigator, configurable: true })
  for (const key of ['Event', 'MouseEvent', 'KeyboardEvent', 'InputEvent', 'Element', 'HTMLElement', 'Node', 'MutationObserver', 'getComputedStyle']) {
    global[key] = window[key]
  }
  window.HTMLElement.prototype.focus = () => {}
  for (const name of ['oninput', 'onchange']) {
    if (!(name in window.HTMLElement.prototype)) {
      Object.defineProperty(window.HTMLElement.prototype, name, { configurable: true, writable: true, value: null })
    }
  }
  class EventSourceStub {
    addEventListener() {}
    close() {}
  }
  global.EventSource = EventSourceStub
  window.EventSource = EventSourceStub
  global.WebSocket = ForbiddenSocket
  window.WebSocket = ForbiddenSocket

  const state = {
    ok: true,
    version: '0.6',
    device: 'probe',
    auto: true,
    data_dir: 'C:\\state',
    settings: { autostart: false, rate_up: 0, rate_down: 0 },
    tasks: [],
  }
  const self = {
    ok: true,
    plugin: 'dsh-winsync',
    version: '0.6.0',
    label: '0.6.0',
    commit: 'test',
    commitSource: 'package.json',
    dirty: false,
    dir: 'C:\\repo\\dsh-winsync',
  }

  global.fetch = async (url, init = {}) => {
    const target = String(url)
    let body = { ok: true }
    if (target.includes('/probe')) {
      probes.push(JSON.parse(init.body ?? '{}'))
      body = { ok: true, result: probeReply }
    } else if (target.includes('/state')) {
      body = state
    } else if (target.includes('/version')) {
      body = self
    }
    return { ok: true, status: 200, text: async () => JSON.stringify(body) }
  }
  window.fetch = global.fetch
  return window
}

async function mount(window, React, createRoot) {
  let captured
  window.__ModuleLoader__ = { load: (options) => (captured = options) }
  new Function('window', readFileSync(BUNDLE, 'utf8'))(window)
  const plugin = captured.factory(() => React)
  const registered = []
  plugin.apply({
    layout: { selectPanel() {} },
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
    await wait(20)
  })
  return root
}

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
const text = () => document.body.textContent ?? ''

async function click(node) {
  assert.ok(node, '要点击的元素不存在')
  await React.act(async () => {
    node.dispatchEvent(new window.MouseEvent('click', { bubbles: true }))
    await wait(20)
  })
}

async function typeInto(node, value) {
  // 必须用原型上的原生 setter：React 在实例上包了一层 tracker
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set
  await React.act(async () => {
    setter.call(node, value)
    node.dispatchEvent(new window.Event('input', { bubbles: true }))
    await wait(20)
  })
}

const byText = (selector, needle) => [...document.querySelectorAll(selector)].find((n) => (n.textContent ?? '').includes(needle))
const nextButton = () => byText('.ws-btn', '下一步')
const settle = () => React.act(async () => { await wait(900) })

/* ------------------------------------------------------------ 开始 */

// React 必须在补齐 oninput 探针之后加载
const win = setup()
const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')

await mount(win, React, createRoot)

await click(byText('.ws-nav-item', '创建同步任务'))
assert.ok(document.querySelector('.ws-modal'), '向导没打开')

const serverInput = document.querySelector('input[placeholder^="192.168"]')
assert.ok(serverInput, '没找到中继地址输入框')
const roomInput = [...document.querySelectorAll('.ws-modal input')].find((n) => /^k[0-9a-f]{32}$/i.test(n.value))
assert.ok(roomInput, '房间码没有默认生成（应是 k 开头的随机码）')
const room = roomInput.value

// 地址为空时不该自检
assert.equal(probes.length, 0, '地址为空时不该发自检请求')
assert.match(text(), /尚未自检/, '空地址时的状态文案不对')

// 填地址 → 防抖后自动自检
await typeInto(serverInput, '127.0.0.1:18443')
await settle()
assert.equal(probes.length, 1, `应当只发一次自检，实际 ${probes.length}`)
assert.equal(probes[0].server, 'ws://127.0.0.1:18443/ws', `自检地址没补全：${probes[0].server}`)
assert.equal(probes[0].workspace, room, '自检没带房间码')
assert.equal(probes[0].token, room, '密钥留空时应当跟随房间码')
assert.match(text(), /中继已接受这组配置/, '握手成功后没显示「中继已接受」')
assert.equal(nextButton().disabled, false, '自检通过后「下一步」必须可点')
assert.equal(sockets.length, 0, '不应该在浏览器里连中继')
console.log('PASS 填地址自动调 daemon 自检（带 workspace + token），浏览器不开 WS')

// 自检通过 ≠ 对端在线：这里没有任何「等对端」的概念，照样能往下走
await React.act(async () => {
  await wait(3000)
})
assert.equal(nextButton().disabled, false, '没有对端在线也必须能继续（以前就是卡在这里）')
console.log('PASS 不要求对端在线：自检通过即可继续')

// 中继拒绝密钥 → 明确说原因，并拦住
probeReply = { state: 'denied', message: '中继拒绝了这组密钥：房间码或密钥与中继配置不一致', elapsed_ms: 9 }
await click(byText('.ws-btn', '重新自检'))
await settle()
assert.match(text(), /中继拒绝了密钥/, '被 401 时没有明确说「密钥不对」')
assert.equal(nextButton().disabled, true, '配置被中继拒绝时应当拦住「下一步」')
assert.match(nextButton().title, /密钥/, '禁用原因没写在 title 上')
console.log('PASS 401 明确提示「密钥不对」，并拦住下一步')

// 连不上 → 同样拦住，并提示检查端口
probeReply = { state: 'unreachable', message: '连不上 ws://127.0.0.1:18443/ws', elapsed_ms: 2000 }
await click(byText('.ws-btn', '重新自检'))
await settle()
assert.match(text(), /连不上中继/, '连不上时没有明确提示')
assert.equal(nextButton().disabled, true, '连不上中继时应当拦住「下一步」')
console.log('PASS 连不上中继时拦住下一步')

// 恢复 → 可以继续进第二步
probeReply = { state: 'accepted', message: '中继已接受：地址、房间码与密钥都对', elapsed_ms: 11 }
await click(byText('.ws-btn', '重新自检'))
await settle()
assert.equal(nextButton().disabled, false, '恢复正常后仍被禁用')
await click(nextButton())
assert.match(text(), /本机同步目录/, '没进到第二步')
console.log('PASS 自检恢复后可以进第二步')

// 回到第一步，改「加入已有同步」：粘邀请串要带出 server / 房间 / 密钥
await click(byText('.ws-btn', '上一步'))
await click(byText('.ws-tab', '加入已有同步'))
const joinInput = [...document.querySelectorAll('.ws-modal input')].find((n) => (n.placeholder ?? '').includes('workspace='))
assert.ok(joinInput, '没找到邀请串输入框')
await typeInto(joinInput, 'ws://10.0.0.5:19000/ws?workspace=kdeadbeefcafe&token=s3cret')
await settle()
const joinServer = [...document.querySelectorAll('.ws-modal input')].find((n) => n.value === '10.0.0.5:19000')
assert.ok(joinServer, '粘邀请串没有带出中继地址')
const joinRoom = [...document.querySelectorAll('.ws-modal input')].find((n) => n.value === 'kdeadbeefcafe')
assert.ok(joinRoom, '粘邀请串没有带出房间码')
const last = probes[probes.length - 1]
assert.equal(last.workspace, 'kdeadbeefcafe', '自检用的房间码不对')
assert.equal(last.token, 's3cret', '自定义密钥没从邀请串里解出来')
assert.equal(last.server, 'ws://10.0.0.5:19000/ws', '自检用的地址不对')
console.log('PASS 粘邀请串带出 server / 房间 / 密钥，自检用的就是这三样')

// 旧版 6 位配对码仍然认，但要展开成 pair-xxxxxx
await typeInto(joinInput, '123456')
await settle()
const legacy = [...document.querySelectorAll('.ws-modal input')].find((n) => n.value === 'pair-123456')
assert.ok(legacy, '6 位旧配对码没有展开成 pair-123456')
assert.equal(probes[probes.length - 1].workspace, 'pair-123456', '旧配对码的自检房间不对')
console.log('PASS 旧 6 位配对码兼容为 pair-xxxxxx 房间')

// 关掉向导
await click(byText('.ws-btn', '取消'))
assert.equal(document.querySelector('.ws-modal'), null, '向导没有关闭')
assert.equal(sockets.length, 0, '整个流程都不该有浏览器直连中继')
console.log('PASS 关闭向导，全程 0 条浏览器 WebSocket')

console.log('pairing: 全部通过')
