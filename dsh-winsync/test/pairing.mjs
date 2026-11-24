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
const probed = [] // 每次 POST /api/probe 的请求体
let stopCount = 0
let presence = { state: 'waiting', peer: '', error: '', room: '' }
// /api/probe 的结论由用例现场设置
let probeResult = { state: 'accepted', elapsed_ms: 9 }

/** 浏览器里一次 WebSocket 都不该建：配对的网络动作全部在 daemon 侧。 */
const sockets = []
class ForbiddenSocket {
  constructor(url) {
    sockets.push(url)
    throw new Error(`不应在浏览器里连中继：${url}`)
  }
}

/** 「复制邀请」写进来的文本，用来核对邀请串格式。 */
const clipboard = []

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
  Object.defineProperty(window.navigator, 'clipboard', {
    value: { writeText: async (value) => clipboard.push(String(value)) },
    configurable: true,
  })
  // 每个用例从「这台机器什么都没记过」开始
  window.localStorage.clear()
  // 浏览器里 globalThis 就是 window；jsdom 下要手动把 localStorage 挂上去
  Object.defineProperty(global, 'localStorage', { value: window.localStorage, configurable: true })

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
    } else if (target.includes('/probe')) {
      probed.push(JSON.parse(init.body ?? '{}'))
      body = { ok: true, result: probeResult }
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

const byClass = (cls) => document.querySelector(`.ws-modal ${cls}`)
const inputs = (predicate) => [...document.querySelectorAll('.ws-modal input')].filter(predicate)
const serverInput = byClass('input[placeholder^="192.168"]')
const channelInput = byClass('input.ws-channel')
assert.ok(serverInput, '没找到中继地址输入框')
assert.ok(channelInput, '没找到通道码输入框')

/* 1. 默认形态：只露 6 位通道码，房间码/密钥/邀请串都收起来 */

assert.match(channelInput.value, /^\d{6}$/, '通道码应当默认生成 6 位数字')
assert.equal(channelInput.disabled, false, '还没配对时通道码应当可输入')
assert.equal(
  inputs((n) => /^k[0-9a-f]{32}$/i.test(n.value)).length,
  0,
  '房间码不该默认出现在界面上（应在高级配置里且打码）',
)
// 高级配置只有一个「展开」入口，内容（房间码/密钥/邀请串）默认不渲染
assert.ok(byText('.ws-link', '高级配置'), '缺少「高级配置」入口')
assert.equal(
  document.querySelectorAll('.ws-modal input[type="password"]').length,
  0,
  '高级配置收起时不该渲染打码字段',
)
console.log('PASS 默认只露 6 位通道码，房间码/密钥/邀请串收在高级配置')

/* 2. 空地址不发起配对，且拦住下一步 */

assert.equal(started.length, 0, '地址为空时不该发起配对')
assert.match(text(), /先填 6 位通道码|还没开始配对/, '空地址时的状态文案不对')
assert.equal(nextButton().disabled, true, '还没配对时「下一步」就该是禁用的')
assert.match(nextButton().textContent, /待配对/, '禁用时按钮文案要写明原因')
console.log('PASS 空地址不发起配对，「下一步」默认禁用')

/* 3. 填地址 → 防抖后自动进配对通道，带的是通道码而不是房间码 */

const channel = channelInput.value
// 从高级配置里把房间码取出来，后面要核对 announce
await click(byText('.ws-link', '高级配置'))
const roomInput = inputs((n) => /^k[0-9a-f]{32}$/i.test(n.value))[0]
assert.ok(roomInput, '展开高级配置后仍看不到房间码')
assert.equal(roomInput.type, 'password', '房间码默认必须是密文')
const room = roomInput.value
await click(byText('.ws-link', '高级配置')) // 收起，回到默认形态

await typeInto(serverInput, '127.0.0.1:18443')
await settle()
assert.equal(started.length, 1, `应当只发起一次配对，实际 ${started.length}`)
assert.equal(started[0].server, 'ws://127.0.0.1:18443/ws', `配对地址没补全：${started[0].server}`)
assert.equal(started[0].workspace, channel, '配对通道应当是 6 位通道码')
assert.equal(started[0].token, channel, '配对通道的密钥应当跟随通道码')
assert.match(
  String(started[0].announce),
  /^(k[0-9a-f]{32})\|\1$/i,
  `创建端没把房间码+密钥挂到见面礼上：${started[0].announce}`,
)
assert.equal(started[0].announce.split('|')[0], room, '见面礼里的房间码与界面上的不一致')
assert.equal(sockets.length, 0, '不应该在浏览器里连中继')
console.log('PASS 填地址自动进配对通道（6 位码），并把自己的房间配置挂上去')

/* 3b. 「测试地址」：超时必须说清是网络/地址，不能拿「端口填第二栏」误导人 */

probeResult = {
  state: 'unreachable',
  message:
    '连不上 ws://10.90.45.64:38443/ws：IO error: 由于连接方在一段时间后没有正确答复或连接主机没有反应，连接尝试失败。 (os error 10060)',
}
await click(byText('.ws-btn', '测试地址'))
await React.act(async () => {
  await wait(60)
})
assert.equal(probed.length, 1, '没发起 /probe')
assert.equal(probed[0].server, 'ws://127.0.0.1:18443/ws', `探测地址不对：${probed[0].server}`)
assert.match(text(), /没有任何响应/, '超时时没说是「没有响应」')
assert.match(text(), /笔误/, '超时时没提示可能是地址写错')
assert.ok(!/端口填在第二栏/.test(text()), '超时不该再甩「端口填在第二栏」这种误导')

// 端口通了但没人监听：这时才该怀疑端口号
probeResult = { state: 'unreachable', message: '... (os error 10061) 由于目标计算机积极拒绝' }
await click(byText('.ws-btn', '测试地址'))
await React.act(async () => {
  await wait(60)
})
assert.match(text(), /没人监听/, '被拒绝时没说是端口没人监听')

// 通了
probeResult = { state: 'accepted', elapsed_ms: 9 }
await click(byText('.ws-btn', '测试地址'))
await React.act(async () => {
  await wait(60)
})
assert.match(text(), /地址通了/, '探测通过时没给出明确结论')
console.log('PASS 测试地址：超时/被拒/通过 三种结论分别说清')

/* 4. 只有我一端 → 明确等待，且拦住下一步 */

assert.match(text(), /正在等对方/, '等待对端时没给出明确文案')
assert.equal(nextButton().disabled, true, '还没配上时不能往下走')
assert.match(nextButton().title, /配对/, '禁用原因没写在 title 上')
assert.ok(byText('.ws-link', '先建我这边'), '缺少「对方已建任务」的显式出口')
console.log('PASS 等待对端时拦住下一步，并给出显式出口')

/* 5. 对端进来 → 配对通过，通道码置灰锁死，下一步放行 */

await applyPresence({ state: 'paired', peer: 'peer-other-side' })
assert.match(text(), /配对已通过/, '配对成功后没显示「配对已通过」')
assert.match(text(), /peer-oth/, '没显示对端设备号')
assert.equal(nextButton().disabled, false, '配对成功后「下一步」必须可点')
assert.equal(channelInput.disabled, true, '配对通过后通道码必须置灰，不让再改')
assert.match(text(), /通道码锁定/, '通道码锁住后没说明为什么不能改')
assert.equal(win.localStorage.getItem('winsync.relay'), '127.0.0.1:18443', '配对成功后应当记住中继地址')
console.log('PASS 配对通过 → 通道码置灰锁定，「下一步」放行，并记住中继地址')

/* 6. 对端离开 → 仍然算配上过：门禁不回退（「对端连上后按钮点不了」的根因） */

// 不点「重新配对」，只让轮询看到对端走了——真实情况就是这样：
// 对端点了下一步、或它的守护断线重连，旁路房间里就没它了
presence = { ...presence, state: 'waiting', peer: '' }
await React.act(async () => {
  await wait(1700)
})
assert.match(text(), /已配对过/, '对端离开后没说明「已配对过」')
assert.equal(nextButton().disabled, false, '配上过之后对端离开，不该再把「下一步」关回去')
assert.equal(channelInput.disabled, true, '通道码仍应锁着，别让人改了对不上')
console.log('PASS 配对后对端离开 → 门禁不回退，「下一步」仍可点')

/* 7. 中继拒绝 → 说清是配对通道的事 */

await applyPresence({
  state: 'denied',
  error: '中继拒绝了配对通道：中继若开了房间级密钥，配对房间要单独放行',
})
assert.match(text(), /中继拒绝了配对通道/, '被 401 时没有明确指出是配对通道')
assert.equal(nextButton().disabled, true, '配置被中继拒绝时应当拦住「下一步」')
assert.match(nextButton().title, /SYNC_ROOMS|密钥/, '禁用原因没写在 title 上')
console.log('PASS 401 明确指出配对通道被拒，并拦住下一步')

/* 8. 连不上 → 同样拦住，并提示检查端口 */

await applyPresence({ state: 'unreachable', error: '连不上 ws://127.0.0.1:18443/ws' })
assert.match(text(), /连不上中继/, '连不上时没有明确提示')
assert.equal(nextButton().disabled, true, '连不上中继时应当拦住「下一步」')
console.log('PASS 连不上中继时拦住下一步')

/* 9. 显式出口：对方已经建好任务，允许先建我这边 */

await applyPresence({ state: 'waiting', peer: '' })
await click(byText('.ws-link', '先建我这边'))
assert.equal(nextButton().disabled, false, '点了「先建我这边」之后应当放行')
assert.match(text(), /已跳过配对等待/, '跳过配对后没有说明')
console.log('PASS 显式出口「先建我这边」可以放行')

/* 10. 重新配对会取消跳过，恢复门禁 */

await applyPresence({ state: 'waiting', peer: '' })
assert.equal(nextButton().disabled, true, '重新配对后应当恢复门禁')

/* 11. 配对通过 → 进第二步，并停掉守护（别继续占位置） */

await applyPresence({ state: 'paired', peer: 'peer-other-side' })
await click(nextButton())
assert.match(text(), /本机同步目录/, '没进到第二步')
assert.ok(stopCount > 0, '离开第一步没停掉配对守护，会一直占着房间')
console.log('PASS 配对通过后进第二步，并停掉配对守护')

/* 12. 加入端：通道码 + 对端送来的房间配置 */

// 加入端此时还没有任何配置，配对通道应当仍在等对端
presence = { state: 'waiting', peer: '', error: '', room: '' }
await click(byText('.ws-btn', '上一步'))
await click(byText('.ws-tab', '加入已有同步'))
await React.act(async () => {
  await wait(60)
})
const joinChannel = byClass('input.ws-channel')
assert.equal(joinChannel.value, '', '切到「加入已有同步」时不该预填别人的通道码')
assert.match(text(), /先填 6 位通道码/, '通道码为空时状态文案不对')

await typeInto(joinChannel, '482917')
await settle()
const joinStarted = started[started.length - 1]
assert.equal(joinStarted.workspace, '482917', '加入端应当用填的 6 位通道码进配对通道')
assert.equal(joinStarted.announce, '', '加入端不该广播自己的配置')
assert.equal(joinChannel.disabled, false, '还没配对时通道码不该锁')
console.log('PASS 加入端用 6 位通道码进配对通道，且不广播配置')

/* 13. 「复制通道码」只复制 6 位数字（邀请串已经不进界面了） */

await click(byText('.ws-btn', '复制通道码'))
assert.equal(clipboard[clipboard.length - 1], '482917', `复制到的不是 6 位通道码：${clipboard[clipboard.length - 1]}`)
console.log('PASS 复制通道码 = 只复制 6 位数字')

/* 14. 对端（创建端）的配置从见面礼里接过来 */

const carriedRoom = `k${'deadbeef'.repeat(4)}`
const hex = Buffer.from(`${carriedRoom}|s3cret`, 'utf8').toString('hex')
presence = { state: 'paired', peer: `ui-7f2a4c91e0~${hex}`, error: '', room: '482917' }
await React.act(async () => {
  await wait(60)
  // 轮询 1.5s 一次，这里等一轮
  await wait(1600)
})
assert.match(text(), /配对已通过/, '对端在线时应当显示配对通过')
assert.match(text(), /自动带过来/, '没说明配置是从对端带过来的')
await click(byText('.ws-link', '高级配置'))
const adoptedRoom = inputs((n) => n.value === carriedRoom)[0]
assert.ok(adoptedRoom, '没从见面礼里解出房间码')
assert.equal(adoptedRoom.type, 'password', '接过来的房间码也必须是密文')
const adoptedToken = inputs((n) => n.value === 's3cret')[0]
assert.ok(adoptedToken, '没从见面礼里解出密钥')
assert.equal(byClass('input.ws-channel').disabled, true, '配对通过后通道码必须锁住')
console.log('PASS 加入端从配对通道收到房间码与密钥，且保持密文')

/* 15. 中继地址拆成 IP 与端口两栏，整串粘进来也能自动拆 */

assert.equal(inputs((n) => n.value === '127.0.0.1')[0]?.value, '127.0.0.1', 'IP 栏没带上主机')
assert.equal(inputs((n) => n.value === '18443')[0]?.value, '18443', '端口栏没带上端口')
assert.equal(inputs((n) => (n.placeholder ?? '').includes('pair='))[0], undefined, '邀请串输入框应当已经移除')
console.log('PASS 中继地址分两栏，整串粘贴自动拆开，邀请串输入框已移除')

/* 16. 端口只认数字，改端口后配对地址跟着变 */

const portInput = inputs((n) => (n.placeholder ?? '').includes('端口'))[0]
assert.ok(portInput, '没找到端口输入框')
await typeInto(portInput, '19000')
await settle()
assert.equal(started[started.length - 1].server, 'ws://127.0.0.1:19000/ws', '改端口后配对地址没跟着变')
await typeInto(portInput, '1a9')
assert.equal(portInput.value, '19', '端口栏应当只收数字')
console.log('PASS 端口栏只收数字，改端口后连接地址同步更新')

/* 17. 关掉向导：守护必须停掉 */

const stopsBefore = stopCount
await click(byText('.ws-btn', '取消'))
assert.equal(document.querySelector('.ws-modal'), null, '向导没有关闭')
await React.act(async () => {
  await wait(50)
})
assert.ok(stopCount > stopsBefore, '关向导时没停掉配对守护')
assert.equal(sockets.length, 0, '整个流程都不该有浏览器直连中继')
console.log('PASS 关闭向导会停掉守护，全程 0 条浏览器 WebSocket')

/* 18. 中继地址记住，下次新建时 IP 与端口两栏都带出来 */

const savedRelay = win.localStorage.getItem('winsync.relay')
assert.ok(savedRelay, '中继地址没被记住')
const cut = savedRelay.lastIndexOf(':')
const savedHost = cut > 0 ? savedRelay.slice(0, cut) : savedRelay
const savedPort = cut > 0 ? savedRelay.slice(cut + 1) : ''
await click(byText('.ws-nav-item', '创建同步任务'))
const prefilledHost = document.querySelector('.ws-modal input[placeholder^="192.168"]')
assert.ok(prefilledHost, '没找到中继地址输入框')
assert.equal(prefilledHost.value, savedHost, `再次新建时没带出上次的 IP（实际「${prefilledHost.value}」）`)
const prefilledPort = inputs((n) => (n.placeholder ?? '').includes('端口'))[0]
assert.equal(prefilledPort.value, savedPort, `再次新建时没带出上次的端口（实际「${prefilledPort.value}」）`)
assert.match(text(), /上次用的中继地址/, '带出的地址没有说明来历')
await click(byText('.ws-btn', '取消'))
console.log('PASS 中继地址记在本机，下次新建 IP 与端口都带出')

/* 19. 记的只是地址：房间码、密钥、通道码都不落盘 */

const dumped = JSON.stringify({ ...win.localStorage })
assert.ok(!/token|secret|k[0-9a-f]{32}|pair=/.test(dumped), `localStorage 里不该有凭据：${dumped}`)
console.log('PASS localStorage 里只有中继地址，没有房间码/密钥/通道码')

console.log('pairing: 全部通过')
