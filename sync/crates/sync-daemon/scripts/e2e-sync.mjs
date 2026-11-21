/**
 * 端到端：真 relay + 真 daemon + 真 CLI，跑一次双向同步。
 *
 * 这是 P1.4 的验收脚本，也是 P4 联调的第一块拼图：三个进程都是仓库里真实的二进制，
 * 没有任何 mock——daemon 必须靠 session::run 把文件真的搬过去。
 *
 * 用法：node scripts/e2e-sync.mjs [--keep]
 * 前置：cargo build --bins（或至少 sync-server / sync-daemon / sync-client）
 */

import { spawn } from 'node:child_process'
import fs from 'node:fs'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'

const repo = path.resolve(import.meta.dirname, '../../..')
const bin = (name) => path.join(repo, 'target', 'debug', `${name}.exe`)
const keep = process.argv.includes('--keep')

const work = path.join(os.tmpdir(), `ws-e2e-${Date.now()}`)
const daemonDir = path.join(work, 'daemon')
const cliDir = path.join(work, 'cli')
const rootA = path.join(daemonDir, 'root')
const rootB = path.join(cliDir, 'root')
const PORT = 18443
const TOKEN = 'e2e-token'
const WORKSPACE = 'e2e'

fs.mkdirSync(rootA, { recursive: true })
fs.mkdirSync(path.join(rootA, 'sub'), { recursive: true })
fs.mkdirSync(rootB, { recursive: true })
fs.writeFileSync(path.join(rootA, 'from-daemon.txt'), 'hello from daemon')
fs.writeFileSync(path.join(rootA, 'sub', 'nested.txt'), 'nested')
fs.writeFileSync(path.join(rootB, 'from-cli.txt'), 'hello from cli')

const children = []
const log = (label, chunk) => {
  const text = chunk.toString().trim()
  if (text) console.log(`[${label}] ${text}`)
}
const spawnTracked = (label, command, args, options) => {
  const child = spawn(command, args, { ...options, windowsHide: true })
  child.stdout?.on('data', (c) => log(label, c))
  child.stderr?.on('data', (c) => log(label, c))
  children.push({ label, child })
  return child
}
const cleanup = () => {
  for (const { child } of children) {
    try {
      child.kill()
    } catch {}
  }
}
process.on('exit', cleanup)

const waitPort = async (port, timeoutMs) => {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const ok = await new Promise((resolve) => {
      const socket = net.connect({ port, host: '127.0.0.1' })
      socket.once('connect', () => {
        socket.destroy()
        resolve(true)
      })
      socket.once('error', () => resolve(false))
    })
    if (ok) return
    await new Promise((r) => setTimeout(r, 200))
  }
  throw new Error(`端口 ${port} 没起来`)
}
const waitFile = async (file, timeoutMs) => {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (fs.existsSync(file)) return
    await new Promise((r) => setTimeout(r, 100))
  }
  throw new Error(`等不到 ${file}`)
}

let exitCode = 0
try {
  spawnTracked('relay', bin('sync-server'), [], {
    env: { ...process.env, SYNC_TOKEN: TOKEN, SYNC_ADDR: `127.0.0.1:${PORT}` },
  })
  await waitPort(PORT, 10000)
  console.log('PASS relay 就绪')

  spawnTracked('daemon', bin('sync-daemon'), [], {
    env: { ...process.env, WINSYNC_STATE_DIR: daemonDir },
  })
  const handshake = path.join(daemonDir, 'daemon.json')
  await waitFile(handshake, 10000)
  const { port, token } = JSON.parse(fs.readFileSync(handshake, 'utf8'))
  const base = `http://127.0.0.1:${port}`
  const call = async (pathname, init = {}) => {
    const response = await fetch(`${base}${pathname}`, {
      ...init,
      headers: {
        authorization: `Bearer ${token}`,
        ...(init.body ? { 'content-type': 'application/json' } : {}),
      },
    })
    return { status: response.status, body: await response.json().catch(() => null) }
  }
  console.log('PASS daemon 就绪')

  const created = await call('/api/tasks', {
    method: 'POST',
    body: JSON.stringify({
      server: `ws://127.0.0.1:${PORT}/ws`,
      token: TOKEN,
      workspace: WORKSPACE,
      root: rootA,
      name: 'e2e',
    }),
  })
  if (created.status !== 200) throw new Error(`建任务失败：${JSON.stringify(created.body)}`)
  const taskId = created.body.task.id
  console.log(`PASS 任务已建 ${taskId}`)

  // 先让 CLI 连上并在房间里等
  spawnTracked(
    'cli',
    bin('sync-client'),
    ['--wait', '40', 'sync'],
    {
      env: {
        ...process.env,
        WINSYNC_STATE_DIR: cliDir,
        SYNC_SERVER: `ws://127.0.0.1:${PORT}/ws`,
        SYNC_TOKEN: TOKEN,
        SYNC_WORKSPACE: WORKSPACE,
        SYNC_ROOT: rootB,
      },
    },
  )
  await new Promise((r) => setTimeout(r, 2500))

  // 订阅 SSE，同时触发同步
  const controller = new AbortController()
  const sse = await fetch(`${base}/api/events`, {
    headers: { authorization: `Bearer ${token}` },
    signal: controller.signal,
  })
  const events = []
  const reader = sse.body.getReader()
  const decoder = new TextDecoder()
  const pump = (async () => {
    try {
      for (;;) {
        const { value, done } = await reader.read()
        if (done) break
        const chunk = decoder.decode(value, { stream: true })
        for (const block of chunk.split('\n\n')) {
          const name = /event: (.+)/.exec(block)?.[1]
          const data = /data: (.+)/.exec(block)?.[1]
          if (name) events.push({ name, data })
        }
        if (events.some((e) => e.name === 'done' && String(e.data).includes(taskId))) break
      }
    } catch {}
  })()

  const started = await call(`/api/tasks/${taskId}/sync`, { method: 'POST' })
  if (started.status !== 200) throw new Error(`触发同步失败：${JSON.stringify(started.body)}`)

  const doneDeadline = Date.now() + 60000
  while (Date.now() < doneDeadline) {
    if (events.some((e) => e.name === 'done' && String(e.data).includes(taskId))) break
    await new Promise((r) => setTimeout(r, 250))
  }
  controller.abort()
  await pump.catch(() => {})

  const names = events.map((e) => e.name)
  const done = events.find((e) => e.name === 'done' && String(e.data).includes(taskId))
  if (!done) throw new Error(`没等到 done 事件，收到：${names.slice(-10).join(',')}`)
  console.log(`PASS SSE 收到 ${names.filter((n) => n === 'log').length} 条 log + done：${done.data}`)
  if (JSON.parse(done.data).error) throw new Error(`同步失败：${done.data}`)

  // 两个方向都要真的落地
  const gotByCli = ['from-daemon.txt', 'sub/nested.txt'].every((rel) =>
    fs.existsSync(path.join(rootB, rel)),
  )
  const gotByDaemon = fs.existsSync(path.join(rootA, 'from-cli.txt'))
  if (!gotByCli) throw new Error('CLI 侧没收到 daemon 的文件')
  if (!gotByDaemon) throw new Error('daemon 侧没收到 CLI 的文件')
  console.log('PASS 双向文件都落盘')

  const activities = await call(`/api/activities?task=${taskId}&limit=50`)
  if ((activities.body?.rows?.length ?? 0) < 2) {
    throw new Error(`活动记录太少：${JSON.stringify(activities.body)}`)
  }
  console.log(`PASS 记录 ${activities.body.rows.length} 条`)

  const state = await call('/api/state')
  const rt = state.body.tasks[0].runtime
  if (rt.running) throw new Error('同步结束后 running 仍为 true')
  if (!rt.last) throw new Error('runtime.last 为空')
  console.log(`PASS 运行时收尾：last=${JSON.stringify(rt.last)}`)

  console.log('E2E 通过')
} catch (e) {
  exitCode = 1
  console.error(`FAIL ${e.message}`)
} finally {
  cleanup()
  if (!keep) {
    await new Promise((r) => setTimeout(r, 500))
    fs.rmSync(work, { recursive: true, force: true })
  } else {
    console.log(`现场保留在 ${work}`)
  }
  setTimeout(() => process.exit(exitCode), 300)
}
