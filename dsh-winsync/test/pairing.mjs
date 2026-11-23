/**
 * 配对交互测试：配对由 daemon 代连，第一步必须先配对成功才能往下走。
 *
 * 旧实现（0.5.x）在浏览器里连中继、挂在 `<房间>::pair` 旁路房间等对端，实测两处走不通：
 *   1. 面板跑在 dsh 页面里，CSP / 混合内容会掐掉到 ws:// 中继的连接，
 *      而同一台机器的 daemon 连得好好的 → 用户看到「配对永远连不上」；
 *   2. 「两端同时在线」被当成门禁时，谁先建好任务谁就不在等待房间，另一端永远等不到人。
 *
 * 现在：daemon 在 `<房间>::pair` 里长期驻守（不占真房间、不打扰同步），
 * 向导轮询 daemon 的 /api/presence 取状态：
 *   waiting  → 显示「正在等对方」，「下一步」禁用（这是明确的产品要求）
 *   paired   → 「配对已通过」，放行
 *   denied / unreachable → 明确报错并禁用
 * 另有显式出口：「对方已经建好任务了？先建我这边」，避免先建的人被永远锁住。
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

const started = [] // 每次 POST /api/presence 的请求体
let stopCount = 0
let presence = { state: 'waiting', peer: '', error: '', room: '' }

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
    version: '0.7',
    device: 'ui-test',
    auto: true,
    data_dir: 'C:\\state',
    settings: { autostart: false, rate_up: 0, rate_down: 0 },
    tasks: [],
  }
  const self = {
    ok: true,
    plugin: 'dsh-winsync',
    version: '0.7.0',
    label: '0.7.0',
    commit: 'test',
    commitSource: 'package.json',
    dirty: false,
    dir: 'C:\\repo\\dsh-winsync',
  }

  global.fetch = async (url, init = {}) => {
    const target = String(url)
    const method = String(init.method ?? 'GET').toUpperCase()
    let body = { ok: true }
    if (target.includes('/presence/stop')) {
      stopCount += 1
      body = { ok: true }
    } else if (target.includes('/presence')) {
      if (method === 'POST') {
        const sent = JSON.parse(init.body ?? '{}')
        started.push(sent)
        presence = { ...presence, room: sent.workspace ?? '', peer: presence.peer ?? '' }
      }
      body = { ok: true, presence }
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
const pairTitle = () => document.querySelector('.ws-pair-title')

/** 改一次配对状态并让它生效：点「重新配对」会重开一次守护，立刻拿到新状态。 */
async function applyPresence(next) {
  presence = { ...presence, ...next }
  const btn = byText('.ws-btn', '重新配对')
  assert.ok(btn, '没找到「重新配对」按钮')
  await click(btn)
  await settle()
}

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

// 地址为空时不该开配对
assert.equal(started.length, 0, '地址为空时不该发起配对')
assert.match(text(), /还没开始配对/, '空地址时的状态文案不对')
assert.equal(nextButton().disabled, true, '还没配对时「下一步」就该是禁用的')
assert.match(nextButton().textContent, /待配对/, '禁用时按钮文案要写明原因')
console.log('PASS 空地址不发起配对，「下一步」默认禁用')

// 填地址 → 防抖后自动开配对
await typeInto(serverInput, '127.0.0.1:18443')
await settle()
assert.equal(started.length, 1, `应当只发起一次配对，实际 ${started.length}`)
assert.equal(started[0].server, 'ws://127.0.0.1:18443/ws', `配对地址没补全：${started[0].server}`)
assert.equal(started[0].workspace, room, '配对没带房间码')
assert.equal(started[0].token, room, '密钥留空时应当跟随房间码')
assert.equal(sockets.length, 0, '不应该在浏览器里连中继')
console.log('PASS 填地址自动开配对（带 workspace + token），浏览器不开 WS')

// 只有我一端 → 明确「正在等对方」，且拦住下一步
assert.match(text(), /正在等对方/, '等待对端时没给出明确文案')
assert.equal(nextButton().disabled, true, '还没配上时不能往下走')
assert.match(nextButton().title, /配对/, '禁用原因没写在 title 上')
assert.ok(byText('.ws-link', '先建我这边'), '缺少「对方已建任务」的显式出口')
console.log('PASS 等待对端时拦住下一步，并给出显式出口')

// 对端进来 → 配对通过，放行
await applyPresence({ state: 'paired', peer: 'peer-other-side' })
assert.match(text(), /配对已通过/, '配对成功后没显示「配对已通过」')
assert.match(text(), /peer-oth/, '没显示对端设备号')
assert.equal(nextButton().disabled, false, '配对成功后「下一步」必须可点')
console.log('PASS 对端进入同一房间 → 配对通过并放行')

// 对端离开 → 回到等待，重新禁用
await applyPresence({ state: 'waiting', peer: '' })
assert.match(text(), /正在等对方/, '对端离开后没退回等待状态')
assert.equal(nextButton().disabled, true, '对端离开后应当重新禁用「下一步」')
console.log('PASS 对端离开 → 退回等待并重新禁用')

// 401 → 明确说原因并拦住
await applyPresence({
  state: 'denied',
  error: '中继拒绝了这组密钥：房间码或密钥与中继配置不一致',
})
assert.match(text(), /中继拒绝了密钥/, '被 401 时没有明确说「密钥不对」')
assert.equal(nextButton().disabled, true, '配置被中继拒绝时应当拦住「下一步」')
assert.match(nextButton().title, /密钥/, '禁用原因没写在 title 上')
console.log('PASS 401 明确提示「密钥不对」，并拦住下一步')

// 连不上 → 同样拦住，并提示检查端口
await applyPresence({ state: 'unreachable', error: '连不上 ws://127.0.0.1:18443/ws' })
assert.match(text(), /连不上中继/, '连不上时没有明确提示')
assert.equal(nextButton().disabled, true, '连不上中继时应当拦住「下一步」')
console.log('PASS 连不上中继时拦住下一步')

// 显式出口：对方已经建好任务，允许先建我这边
await applyPresence({ state: 'waiting', peer: '' })
await click(byText('.ws-link', '先建我这边'))
assert.equal(nextButton().disabled, false, '点了「先建我这边」之后应当放行')
assert.match(text(), /已跳过配对等待/, '跳过配对后没有说明')
console.log('PASS 显式出口「先建我这边」可以放行')

// 重新配对会取消跳过，恢复门禁
await applyPresence({ state: 'waiting', peer: '' })
assert.equal(nextButton().disabled, true, '重新配对后应当恢复门禁')

// 配对通过 → 进第二步，并停掉守护（别继续占位置）
await applyPresence({ state: 'paired', peer: 'peer-other-side' })
await click(nextButton())
assert.match(text(), /本机同步目录/, '没进到第二步')
assert.ok(stopCount > 0, '离开第一步没停掉配对守护，会一直占着房间')
console.log('PASS 配对通过后进第二步，并停掉配对守护')

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
const last = started[started.length - 1]
assert.equal(last.workspace, 'kdeadbeefcafe', '配对用的房间码不对')
assert.equal(last.token, 's3cret', '自定义密钥没从邀请串里解出来')
assert.equal(last.server, 'ws://10.0.0.5:19000/ws', '配对用的地址不对')
console.log('PASS 粘邀请串带出 server / 房间 / 密钥，配对用的就是这三样')

// 旧版 6 位配对码仍然认，但要展开成 pair-xxxxxx
await typeInto(joinInput, '123456')
await settle()
const legacy = [...document.querySelectorAll('.ws-modal input')].find((n) => n.value === 'pair-123456')
assert.ok(legacy, '6 位旧配对码没有展开成 pair-123456')
assert.equal(started[started.length - 1].workspace, 'pair-123456', '旧配对码的配对房间不对')
console.log('PASS 旧 6 位配对码兼容为 pair-xxxxxx 房间')

// 关掉向导：守护必须停掉
const stopsBefore = stopCount
await click(byText('.ws-btn', '取消'))
assert.equal(document.querySelector('.ws-modal'), null, '向导没有关闭')
await React.act(async () => { await wait(50) })
assert.ok(stopCount > stopsBefore, '关向导时没停掉配对守护')
assert.equal(sockets.length, 0, '整个流程都不该有浏览器直连中继')
console.log('PASS 关闭向导会停掉守护，全程 0 条浏览器 WebSocket')

console.log('pairing: 全部通过')
