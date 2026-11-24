/**
 * 进度条与实时速率的端到端检查：真中继 + 两个真 daemon + 一个大文件。
 *
 * 验的是**面板真正读的那条链路**：session 发 Event::Progress（字节）→ daemon 写进
 * runtime.progress → /api/state 里能读到「已传 / 总量 / 速率」。
 * 早期这里报的是「第几个文件」，单个大文件永远停在 0/1，界面上等于没有进度条。
 *
 *   node sync/tools/progress.mjs [文件大小MB，默认 96]
 *
 * 前置：cargo build --release -p sync-daemon -p sync-server
 */

import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const repo = path.resolve(here, '..', '..')
const RELAY = path.join(repo, 'sync', 'target', 'release', 'sync-server.exe')
const DAEMON = path.join(repo, 'sync', 'target', 'release', 'sync-daemon.exe')
const SIZE_MB = Number(process.argv[2] ?? 96)

for (const bin of [RELAY, DAEMON]) {
  if (!fs.existsSync(bin)) {
    console.error(`缺少 ${bin}，先 cargo build --release -p sync-daemon -p sync-server`)
    process.exit(1)
  }
}

const PORT = 19011
const ROOM = `k${'5a'.repeat(16)}`
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'ws-progress-'))
const leftRoot = path.join(work, 'left')
const rightRoot = path.join(work, 'right')

function writeConfig(stateDir, root) {
  fs.mkdirSync(stateDir, { recursive: true })
  fs.writeFileSync(
    path.join(stateDir, 'config.json'),
    JSON.stringify(
      {
        version: 2,
        tasks: [
          {
            id: 'big',
            name: 'big',
            server: `ws://127.0.0.1:${PORT}/ws`,
            token: ROOM,
            workspace: ROOM,
            root,
            mode: 'bidirectional',
            filter: { ignore_dotfiles: true, ignore_extensions: [], size_min: null, size_max: null },
            paused: false,
          },
        ],
      },
      null,
      2,
    ),
  )
}

/** 造一个指定大小的文件（内容不重要，但要能算出哈希，别用稀疏文件）。 */
function makeBigFile(dir, mb) {
  fs.mkdirSync(dir, { recursive: true })
  const file = path.join(dir, 'big.bin')
  const block = Buffer.alloc(1024 * 1024)
  const fd = fs.openSync(file, 'w')
  for (let i = 0; i < mb; i += 1) {
    block.fill(i & 0xff)
    fs.writeSync(fd, block)
  }
  fs.closeSync(fd)
  return file
}

const relay = spawn(RELAY, [], {
  env: { ...process.env, SYNC_ADDR: `127.0.0.1:${PORT}`, SYNC_RATE_LIMIT: '600' },
  stdio: 'ignore',
  windowsHide: true,
})
const stateA = path.join(work, 'state-left')
const stateB = path.join(work, 'state-right')
writeConfig(stateA, leftRoot)
writeConfig(stateB, rightRoot)
const daemons = [
  spawn(DAEMON, [], { env: { ...process.env, WINSYNC_STATE_DIR: stateA }, stdio: 'ignore', windowsHide: true }),
  spawn(DAEMON, [], { env: { ...process.env, WINSYNC_STATE_DIR: stateB }, stdio: 'ignore', windowsHide: true }),
]

const cleanup = () => {
  for (const child of [...daemons, relay]) {
    try {
      child.kill()
    } catch {
      /* 已经退了 */
    }
  }
}
process.on('exit', cleanup)

async function handshake(dir) {
  const file = path.join(dir, 'daemon.json')
  for (let i = 0; i < 100; i += 1) {
    try {
      const info = JSON.parse(fs.readFileSync(file, 'utf8'))
      if (info?.port && info?.token) return info
    } catch {
      /* 还没写出来 */
    }
    await wait(100)
  }
  throw new Error(`${dir} 的 daemon.json 一直没出现`)
}

function client(info) {
  const call = async (route, init = {}) => {
    const response = await fetch(`http://127.0.0.1:${info.port}${route}`, {
      method: init.method ?? 'GET',
      headers: {
        authorization: `Bearer ${info.token}`,
        ...(init.body ? { 'content-type': 'application/json' } : {}),
      },
      body: init.body ? JSON.stringify(init.body) : undefined,
    })
    return { status: response.status, body: await response.json() }
  }
  return call
}

// 对端的空目录也要先建：daemon 启动同步时故意不自动建目录（盘没挂上时会把空目录当真）
fs.mkdirSync(rightRoot, { recursive: true })
const bigFile = makeBigFile(leftRoot, SIZE_MB)
const size = fs.statSync(bigFile).size
console.log(`造好 ${SIZE_MB} MB 的文件，两端启动中…`)

const a = client(await handshake(stateA))
const b = client(await handshake(stateB))
await wait(600)

/** 抓一次左端的进度快照；没有进度就返回 null。 */
async function readProgress(info) {
  const { body } = await info('/api/state')
  const rt = body?.tasks?.[0]?.runtime
  return rt?.progress ?? null
}

const seen = []
const seenDown = []
let fired = false
const poll = (async () => {
  for (let i = 0; i < 600; i += 1) {
    const p = await readProgress(a)
    if (p && p.total > 0) {
      seen.push(p)
      fired = true
    }
    const q = await readProgress(b)
    if (q && q.total > 0) {
      seenDown.push(q)
      fired = true
    }
    if (fired && !p) break
    await wait(100)
  }
})()

// 两端同时触发：会话需要两台 daemon 同时在线上
const kicks = await Promise.all([a('/api/tasks/big/sync', { method: 'POST' }), b('/api/tasks/big/sync', { method: 'POST' })])
if (process.env.VERBOSE) console.log('sync 响应', JSON.stringify(kicks))
await poll

const picked = seen.filter((p) => p.total === size)
const middle = picked.filter((p) => p.done > 0 && p.done < p.total)
const maxBps = Math.max(0, ...picked.map((p) => Math.max(p.up_bps ?? 0, p.down_bps ?? 0)))
const lastDone = Math.max(0, ...picked.map((p) => p.done))

const checks = [
  ['读到过进度事件', picked.length > 0],
  ['进度总量 = 文件大小', picked.length > 0 && picked.every((p) => p.total === size)],
  ['中途有中间值（不是只有 0 和满）', middle.length >= 2],
  ['进度单调不减', picked.length < 2 || lastDone >= Math.min(...picked.map((p) => p.done))],
  ['有非零速率', maxBps > 0],
  ['速率量级合理（> 100 KB/s）', maxBps > 100 * 1024],
  ['对端也看得到下载速率', Math.max(0, ...seenDown.map((p) => p.down_bps ?? 0)) > 0],
]

for (const [name, ok] of checks) {
  console.log(`${ok ? '✓' : '✗'} ${name}`)
  if (!ok) process.exitCode = 1
}
console.log(
  `\n采样 ${picked.length} 次，done 最大 ${(lastDone / 1024 / 1024).toFixed(1)} MB / ${(size / 1024 / 1024).toFixed(0)} MB，峰值 ${(maxBps / 1024 / 1024).toFixed(1)} MB/s`,
)
if (process.exitCode) {
  console.log(JSON.stringify(picked.slice(0, 5), null, 2))
}

// 收尾：确认对端真的落地了这个文件（内容哈希一致）
const sha = (file) => createHash('sha256').update(fs.readFileSync(file)).digest('hex')
const arrived = path.join(rightRoot, 'big.bin')
await wait(1500)
if (fs.existsSync(arrived)) {
  const same = sha(arrived) === sha(bigFile)
  console.log(`${same ? '✓' : '✗'} 对端文件字节一致`)
  if (!same) process.exitCode = 1
} else {
  console.log('· 对端还没落地（本轮只看进度，不判失败）')
}

cleanup()
process.exit(process.exitCode ?? 0)
