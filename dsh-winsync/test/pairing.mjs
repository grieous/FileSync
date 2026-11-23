/**
 * 配对逻辑测试：进配对步骤就「加入房间」，不是点按钮才去问一句。
 *
 * 原来的实现有个错位：生成校验码的那一端**并没有真的进房间**，它只在用户点
 * 「检测配对」时开一条 WS 问一句就走。于是另一端点检测时房间里只有自己，
 * 永远「对端不在线」，向导卡在第一步。
 *
 * 这里用一个假中继（按 sync-server 的真实行为回 PeerStatus）验证新语义：
 *   1. 服务器 + 房间码一有效就发 Hello，房间名走旁路（真房间 + `::pair`），
 *      免得和 daemon 抢那 2 个位置
 *   2. 只连一条，device_id 固定（重连是替换自己，不是占第二个位）
 *   3. 对端进房间 → 自动变「配对已通过」，「下一步」解锁（不用点按钮）
 *   4. 对端离开 → 退回「等待对端」，「下一步」重新禁用
 *   5. 换服务器 / 关闭向导 → 主动离房（旧 socket 关掉）
 *
 * 用法（需要 react + react-dom + jsdom）：
 *   ln -s <有这些依赖的目录> node_modules && node test/pairing.mjs
 */

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

import { JSDOM } from 'jsdom'

globalThis.IS_REACT_ACT_ENVIRONMENT = true

const BUNDLE = process.env.WINSYNC_CLIENT ?? new URL('../lib/client.js', import.meta.url).pathname

/* ------------------------------------------------------------ 假中继 */

const encode = (message) => {
  const body = new TextEncoder().encode(JSON.stringify(message))
  const frame = new Uint8Array(1 + body.length)
  frame[0] = 0
  frame.set(body, 1)
  return frame
}
const decode = (data) => JSON.parse(new TextDecoder().decode(new Uint8Array(data).subarray(1)))

const rooms = new Map() // room -> [socket]
const hellos = [] // 每条 Hello 的原始内容
const sockets = []

function push(socket, message) {
  socket.emit('message', { data: encode(message) })
}

/** 与 sync-server 的 join() 等价：同 device 替换、满 2 即配对、否则推 peer_online=false。 */
function join(socket, room, device) {
  const others = (rooms.get(room) ?? []).filter((s) => s.device !== device)
  const old = (rooms.get(room) ?? []).find((s) => s.device === device)
  if (old && old !== socket) push(old, { t: 'bye', reason: '同一设备重新连接，旧连接被替换' })
  others.push(socket)
  socket.device = device
  socket.room = room
  rooms.set(room, others)

  if (others.length >= 2) {
    for (const peer of others) {
      push(peer, {
        t: 'peer_status',
        peer_online: true,
        peer_device_id: others.find((s) => s !== peer)?.device ?? null,
      })
    }
  } else {
    push(socket, { t: 'peer_status', peer_online: false, peer_device_id: null })
  }
}

function leave(socket) {
  const list = rooms.get(socket.room) ?? []
  const next = list.filter((s) => s !== socket)
  rooms.set(socket.room, next)
  for (const peer of next) push(peer, { t: 'peer_status', peer_online: false, peer_device_id: null })
}

class FakeSocket {
  static CONNECTING = 0
  static OPEN = 1
  static CLOSING = 2
  static CLOSED = 3

  constructor(url) {
    this.url = url
    this.readyState = FakeSocket.CONNECTING
    this.device = ''
    this.room = ''
    this.listeners = new Map()
    sockets.push(this)
    setTimeout(() => {
      this.readyState = FakeSocket.OPEN
      this.emit('open', {})
    }, 0)
  }

  addEventListener(type, fn, options) {
    const list = this.listeners.get(type) ?? []
    const entry = options?.once ? (event) => { fn(event); this.removeEventListener(type, entry) } : fn
    list.push(entry)
    this.listeners.set(type, list)
  }

  removeEventListener(type, fn) {
    this.listeners.set(type, (this.listeners.get(type) ?? []).filter((entry) => entry !== fn))
  }

  emit(type, event) {
    for (const fn of [...(this.listeners.get(type) ?? [])]) fn(event)
  }

  send(frame) {
    if (this.readyState !== FakeSocket.OPEN) throw new Error('socket 未打开')
    const message = decode(frame)
    if (message.t === 'hello') {
      hellos.push({ room: message.workspace_id, device: message.device_id })
      join(this, message.workspace_id, message.device_id)
    }
  }

  close() {
    if (this.readyState === FakeSocket.CLOSED) return
    this.readyState = FakeSocket.CLOSING
    setTimeout(() => {
      if (this.readyState === FakeSocket.CLOSED) return
      this.readyState = FakeSocket.CLOSED
      leave(this)
      this.emit('close', {})
    }, 0)
  }

  get closed() {
    return this.readyState === FakeSocket.CLOSED
  }
}

/** 造一个「对端」，直接以给定房间名进房（模拟另一台机器上的向导）。 */
function externalPeer(room) {
  const socket = new FakeSocket('ws://peer')
  socket.readyState = FakeSocket.OPEN
  join(socket, room, `ext-${Math.random().toString(16).slice(2, 8)}`)
  return socket
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
  global.WebSocket = FakeSocket
  window.WebSocket = FakeSocket

  const state = {
    ok: true,
    version: '0.5',
    device: 'probe',
    auto: true,
    data_dir: 'C:\\state',
    settings: { autostart: false, rate_up: 0, rate_down: 0 },
    tasks: [],
  }
  const self = {
    ok: true,
    plugin: 'dsh-winsync',
    version: '0.5.3',
    label: '0.5.3 · 6567fc7',
    commit: '6567fc7',
    commitSource: 'package.json',
    dirty: false,
    dir: 'C:\\repo\\dsh-winsync',
  }
  global.fetch = async (url) => {
    const target = String(url)
    const body = target.includes('/state') ? state : target.includes('/version') ? self : { ok: true, rows: [] }
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
  // 必须用原型上的原生 setter：React 在实例上包了一层 tracker，走实例 setter 会把
  // tracker 一起更新，React 反而认为「值没变」而不触发 onChange。
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set
  await React.act(async () => {
    setter.call(node, value)
    node.dispatchEvent(new window.Event('input', { bubbles: true }))
    await wait(20)
  })
}

const byText = (selector, needle) => [...document.querySelectorAll(selector)].find((n) => (n.textContent ?? '').includes(needle))
const nextButton = () => byText('.ws-btn', '下一步')

/* ------------------------------------------------------------ 开始 */

// React 必须在「补齐 oninput 探针」之后才加载：它初始化时就把 isInputEventSupported 定死，
// 之后再补属性也不认，受控输入会改走 change 兜底路径，jsdom 里就收不到 onChange。
const win = setup()
const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')

await mount(win, React, createRoot)

// 打开向导
await click(byText('.ws-nav-item', '创建同步任务'))
assert.ok(document.querySelector('.ws-modal'), '向导没打开')

const serverInput = document.querySelector('input[placeholder^="192.168"]')
assert.ok(serverInput, '没找到服务器地址输入框')
const roomInput = [...document.querySelectorAll('.ws-modal input')].find((n) => /^[a-z0-9]{8,}$/i.test(n.value))
assert.ok(roomInput, '没找到房间码输入框（默认应是自动生成的随机码）')
const room = roomInput.value

// 还没填服务器：不该连
assert.equal(sockets.length, 0, '服务器为空时不该建立连接')
assert.match(text(), /尚未加入房间/, '空地址时状态文案不对')

// 填服务器 → debounce 450ms 后自动进房
await typeInto(serverInput, '127.0.0.1:18443')
await React.act(async () => {
  await wait(700)
})

assert.equal(sockets.length, 1, `应当只建一条等待连接，实际 ${sockets.length}`)
assert.equal(hellos.length, 1, '没有发出 Hello')
assert.equal(hellos[0].room, `${room}::pair`, `等待房间应是旁路房间，实际 ${hellos[0].room}`)
assert.match(hellos[0].device, /^dsh-ui-/, '等待连接的 device_id 应是固定的 dsh-ui- 前缀')
assert.match(text(), /已在房间等待对端/, '进房后没有显示「等待对端」')
// 没配对上也要能继续：对方很可能已经建好任务，不能因为等不到就把人锁死
assert.equal(nextButton().disabled, false, '未检测到对端时「下一步」也不该禁用')
assert.match(nextButton().textContent, /未检测到对端/, '按钮没提示「未检测到对端」')
console.log('PASS 填好地址即自动进房（房间走 ::pair 旁路，device_id 固定）')

// 对端进房 → 自动配对通过
let peer
await React.act(async () => {
  peer = externalPeer(`${room}::pair`)
  await wait(30)
})
assert.match(text(), /配对已通过/, '对端进房后没有自动变成「配对已通过」')
assert.equal(nextButton().disabled, false, '配对通过后「下一步」应当解锁')
assert.match(nextButton().textContent, /^下一步$/, '配对通过后按钮文案应当回到「下一步」')
console.log('PASS 对端进房即自动配对通过，「下一步」解锁（无需点检测）')

// 对端离开 → 回到等待
await React.act(async () => {
  peer.close()
  await wait(30)
})
assert.match(text(), /已在房间等待对端/, '对端离开后没有退回等待状态')
assert.match(nextButton().textContent, /未检测到对端/, '对端离开后按钮没退回提示态')
console.log('PASS 对端离开后退回等待，按钮退回提示态')

// 对方已建好任务：真房间里有 daemon，等待房间没人 → 等 3 秒探测后如实说出来
const daemonPeer = externalPeer(room)
await React.act(async () => {
  await wait(3400)
})
assert.match(text(), /对端已建好任务/, '没识别出「对端已在真房间」的情况')
assert.match(text(), /直接继续建任务/, '没给出继续的指引')
assert.equal(nextButton().disabled, false, '识别到对端已建任务后应当可以继续')
await React.act(async () => {
  daemonPeer.close()
  await wait(30)
})
console.log('PASS 等待房间没人时，去真房间探测到「对端已建好任务」')

// 换服务器 → 旧连接应当被关掉，不能两条并存
const first = sockets[0]
await typeInto(serverInput, '127.0.0.1:18444')
await React.act(async () => {
  await wait(700)
})
assert.equal(first.closed, true, '换服务器后旧连接没有关闭（会白占房间位）')
assert.ok(sockets.length >= 2, '换服务器后没有建立新连接')
assert.equal(sockets.filter((s) => !s.closed).length, 1, '同时应当只有一条等待连接')
assert.equal(hellos[hellos.length - 1].room, `${room}::pair`, '换服务器后房间名不该变')
console.log('PASS 换服务器会重建连接，且始终只有一条')

// 关向导 → 离房
await click(byText('.ws-btn', '取消'))
await React.act(async () => {
  await wait(50)
})
assert.ok(sockets.every((s) => s.closed), '关掉向导后还有连接没断（会一直占着房间）')
assert.equal(document.querySelector('.ws-modal'), null, '向导没有关闭')
console.log('PASS 关闭向导会主动离房')

console.log('pairing: 全部通过')
