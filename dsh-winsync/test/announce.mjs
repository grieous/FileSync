/**
 * 配对通道的「见面礼」测试：6 位通道码负责见面，32 位房间码靠它带过去。
 *
 * 这一条链路跨了三段，任何一段断掉，界面上都只会看到「一直等对方」，
 * 光看界面分不出来是哪一段，所以在这里直接压真实的中继 + 真实的 daemon：
 *
 *   向导 POST /api/presence {announce:"房间码|密钥"}
 *     → daemon 把它编进 Hello 的 device_id（`ui-<随机>~<hex>`）
 *     → 中继把对端 device_id 原样放进 PeerStatus.peer_device_id 推回来
 *     → 向导从 presence.peer 里解出房间码与密钥
 *
 * 用法：node test/announce.mjs
 * 前置：cargo build --release -p sync-server -p sync-daemon
 */

import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const repo = path.resolve(here, '..', '..')
const RELAY = path.join(repo, 'sync', 'target', 'release', 'sync-server.exe')
const DAEMON = process.env.WINSYNC_DAEMON_BIN ?? path.join(repo, 'sync', 'target', 'release', 'sync-daemon.exe')
assert.ok(fs.existsSync(RELAY), '先 cargo build --release -p sync-server')
assert.ok(fs.existsSync(DAEMON), '先 cargo build --release -p sync-daemon')

const PORT = 18477
const stateDir = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-ws-announce-'))
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

const relay = spawn(RELAY, [], {
  env: { ...process.env, SYNC_ADDR: `127.0.0.1:${PORT}`, SYNC_RATE_LIMIT: '600' },
  stdio: 'ignore',
})
const daemon = spawn(DAEMON, [], {
  env: { ...process.env, WINSYNC_STATE_DIR: stateDir },
  stdio: 'ignore',
})

const cleanup = () => {
  for (const child of [daemon, relay]) {
    try {
      child.kill()
    } catch {
      /* 已经退了 */
    }
  }
  try {
    fs.rmSync(stateDir, { recursive: true, force: true })
  } catch {
    /* 目录被占用就算了，临时目录 */
  }
}
process.on('exit', cleanup)

/** 等 daemon 写出握手文件（pid/port/token）。 */
async function handshake() {
  const file = path.join(stateDir, 'daemon.json')
  for (let i = 0; i < 100; i += 1) {
    try {
      const info = JSON.parse(fs.readFileSync(file, 'utf8'))
      if (info?.port && info?.token) return info
    } catch {
      /* 还没写出来 */
    }
    await wait(100)
  }
  throw new Error('daemon.json 一直没出现')
}

const info = await handshake()
await wait(300)

async function api(route, init = {}) {
  const response = await fetch(`http://127.0.0.1:${info.port}${route}`, {
    method: init.method ?? 'GET',
    body: init.body ? JSON.stringify(init.body) : undefined,
    headers: {
      accept: 'application/json',
      authorization: `Bearer ${info.token}`,
      ...(init.body ? { 'content-type': 'application/json' } : {}),
    },
  })
  return { status: response.status, body: await response.json() }
}

// 把 Frame 编成中继要的二进制：首字节 0 表示控制帧，后面是 JSON
function frame(payload) {
  const bytes = Buffer.from(JSON.stringify(payload), 'utf8')
  const out = Buffer.alloc(bytes.length + 1)
  out[0] = 0
  bytes.copy(out, 1)
  return out
}

const hex = (text) => Buffer.from(text, 'utf8').toString('hex')

/* ------------------------------------------------------------ 我方：把配置挂上去 */

const room = `k${'ab'.repeat(16)}`
const announce = `${room}|s3cret`
const started = await api('/api/presence', {
  method: 'POST',
  body: { server: `127.0.0.1:${PORT}`, workspace: '482917', token: '482917', announce },
})
assert.equal(started.status, 200, `POST /api/presence 失败：${JSON.stringify(started.body)}`)
assert.equal(started.body.presence.room, '482917', 'presence 记住的应当是 6 位通道码')
// 刚发出时是 connecting，握手完成才变 waiting
let fresh = started.body.presence
for (let i = 0; i < 40 && fresh.state === 'connecting'; i += 1) {
  await wait(100)
  fresh = (await api('/api/presence')).body.presence
}
assert.equal(fresh.state, 'waiting', `还没对端时应当是 waiting，实际 ${fresh.state}`)
console.log('PASS daemon 用 6 位通道码进配对通道')

/* ------------------------------------------------------------ 对端：戴上它自己的配置 */

const peerRoom = `k${'cd'.repeat(16)}`
const peerAnnounce = `${peerRoom}|peer-token`
const peerDevice = `uidev-peer~${hex(peerAnnounce)}`
const seen = []
const socket = new WebSocket(`ws://127.0.0.1:${PORT}/ws?workspace=${encodeURIComponent('482917::pair')}&token=482917`)
socket.binaryType = 'arraybuffer'
const opened = new Promise((resolve, reject) => {
  socket.addEventListener('open', resolve)
  socket.addEventListener('error', () => reject(new Error('对端连不上中继')))
})
await opened
socket.addEventListener('message', (event) => {
  const bytes = Buffer.from(event.data)
  if (bytes[0] !== 0) return
  try {
    seen.push(JSON.parse(bytes.subarray(1).toString('utf8')))
  } catch {
    /* 不是控制帧 */
  }
})
socket.send(
  frame({
    t: 'hello',
    device_id: peerDevice,
    workspace_id: '482917::pair',
    proto_ver: 1,
  }),
)

/* ------------------------------------------------------------ 两端互看 */

let presence = null
for (let i = 0; i < 60; i += 1) {
  const { body } = await api('/api/presence')
  presence = body.presence
  if (presence?.state === 'paired') break
  await wait(100)
}

assert.equal(presence?.state, 'paired', `没能配对：${JSON.stringify(presence)}`)
assert.equal(presence.peer, peerDevice, '从 presence.peer 拿到的不是对端原样的 device id')
console.log('PASS 中继把对端 device id 原样推回，我方读到完整载荷')

// 向导侧的解析规则：`ui-<随机>~<hex>`，`~` 后面就是 `房间码|密钥`
const carried = Buffer.from(/~([0-9a-f]+)$/i.exec(presence.peer)[1], 'hex').toString('utf8')
const [carriedRoom, carriedToken] = carried.split('|')
assert.equal(carriedRoom, peerRoom, '解出来的房间码不对')
assert.equal(carriedToken, 'peer-token', '解出来的密钥不对')
console.log('PASS 房间码与密钥能按约定解出来')

/* ------------------------------------------------------------ 对端也能看到我方配置 */

const peerStatus = seen.find((msg) => msg.t === 'peer_status')
assert.ok(peerStatus, `对端没收到 PeerStatus：${JSON.stringify(seen)}`)
assert.equal(peerStatus.peer_online, true, '对端看到的应当是在线')
const mine = String(peerStatus.peer_device_id ?? '')
assert.match(mine, /^ui-[0-9a-f]+~[0-9a-f]+$/, `我方 device id 格式不对：${mine}`)
const mineCarried = Buffer.from(mine.split('~')[1], 'hex').toString('utf8')
assert.equal(mineCarried, announce, '对端收到的配置与我方广播的不一致')
console.log('PASS 创建端的配置也能送到对端')

/* ------------------------------------------------------------ 收尾 */

socket.close()
await api('/api/presence/stop', { method: 'POST' })
const stopped = await api('/api/presence')
assert.equal(stopped.body.presence.state, 'idle', '停配对后应当回到 idle')
console.log('PASS 停配对后回到 idle')

console.log('announce: 全部通过')
cleanup()
process.exit(0)
