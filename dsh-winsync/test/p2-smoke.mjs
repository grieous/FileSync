/**
 * P2 冒烟：不需要 dsh、不需要浏览器，直接验证宿主半的三件事。
 *
 *   1. `apply(ctx)` 注册了 `/winsync/api` 路由与 4 个工具
 *   2. 通过代理能真的拉起 winSync-daemon，并把 CRUD 一路透传到它
 *   3. 卸载后 daemon 被停掉、daemon.json 被清掉（不留孤儿）
 *
 * 用法：node test/p2-smoke.mjs
 * 前置：cargo build -p sync-daemon（脚本会用仓库里的 target/debug）
 */

import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const repo = path.resolve(here, '..', '..')
const stateDir = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-ws-p2-'))

process.env.WINSYNC_STATE_DIR = stateDir
process.env.WINSYNC_DAEMON_BIN = path.join(repo, 'sync', 'target', 'debug', 'sync-daemon.exe')
assert.ok(fs.existsSync(process.env.WINSYNC_DAEMON_BIN), '先 cargo build -p sync-daemon')

const plugin = await import('../lib/index.js')
assert.equal(plugin.name, 'dsh-winsync')

const effects = []
const routes = []
const tools = new Map()
const ctx = {
  effect(factory, label) {
    const dispose = factory()
    effects.push({ label, dispose })
    return () => {}
  },
  webServer: {
    register(options) {
      routes.push(options)
      return () => {}
    },
  },
  tools: {
    register(definition) {
      tools.set(definition.name, definition)
      return () => {}
    },
  },
}

plugin.apply(ctx)

/* ------------------------------------------------------------ 注册契约 */

assert.equal(routes.length, 1)
assert.equal(routes[0].kind, 'prefix')
assert.equal(routes[0].path, '/winsync/api')
console.log('PASS 注册了 /winsync/api prefix 路由')

assert.deepEqual(
  [...tools.keys()].sort(),
  ['winsync_recent_activity', 'winsync_status', 'winsync_sync_now', 'winsync_tasks'],
)
for (const [name, def] of tools) {
  assert.equal(typeof def.execute, 'function', `${name} 缺 execute`)
  assert.equal(def.parameters?.type, 'object', `${name} 的 parameters 不是 JSON Schema object`)
  assert.equal(typeof def.output?.render, 'function', `${name} 缺 output.render`)
}
console.log('PASS 注册了 4 个 agent 工具，且都是标准 JSON Schema')

/* ------------------------------------------------------------ 起本地 server */

const handler = routes[0].handler
const server = createServer((req, res) => {
  Promise.resolve(handler(req, res)).catch((error) => {
    res.writeHead(500, { 'content-type': 'application/json' })
    res.end(JSON.stringify({ ok: false, error: { code: 'test', message: String(error) } }))
  })
})
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
const base = `http://127.0.0.1:${server.address().port}`

const call = async (route, init = {}) => {
  const response = await fetch(`${base}/winsync/api${route}`, {
    ...init,
    headers: { ...(init.body ? { 'content-type': 'application/json' } : {}), ...init.headers },
  })
  const text = await response.text()
  let body
  try {
    body = JSON.parse(text)
  } catch {
    body = text
  }
  return { status: response.status, body }
}

const ping = await call('/ping')
assert.equal(ping.status, 200)
assert.equal(ping.body.ok, true)
assert.ok(!JSON.stringify(ping.body).includes('token'), 'ping 不能泄露 token')
console.log('PASS /ping 正常且不含 token')

const diag = await call('/diag')
assert.equal(diag.status, 200)
assert.equal(diag.body.health?.status, 200, `daemon 没起来：${JSON.stringify(diag.body)}`)
console.log(`PASS /diag 拉起 daemon pid=${diag.body.health.pid} port=${diag.body.health.port}`)

/* ------------------------------------------------------------ 代理透传 CRUD */

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-ws-p2-root-'))
const created = await call('/tasks', {
  method: 'POST',
  body: JSON.stringify({
    server: 'ws://127.0.0.1:18443/ws',
    token: 'p2-token',
    workspace: 'default',
    root,
    name: 'P2 冒烟',
  }),
})
assert.equal(created.status, 200, JSON.stringify(created.body))
const taskId = created.body.task.id
console.log(`PASS 通过代理建任务 ${taskId}`)

const state = await call('/state')
assert.equal(state.body.tasks.length, 1)
assert.equal(state.body.tasks[0].display_name, 'P2 冒烟')

const badRoot = await call(`/tasks/${taskId}`, {
  method: 'PATCH',
  body: JSON.stringify({ root: path.join(root, 'nope') }),
})
assert.equal(badRoot.status, 409)
assert.equal(badRoot.body.error.code, 'root-changed')
console.log('PASS 代理把 409 root-changed 原样传回')

const notFound = await call('/tasks/nope', { method: 'DELETE' })
assert.equal(notFound.status, 404)
console.log('PASS 未知任务 → 404')

/* ------------------------------------------------------------ SSE 直通 */

const controller = new AbortController()
const sse = await fetch(`${base}/winsync/api/events`, { signal: controller.signal })
assert.equal(sse.status, 200)
const { value } = await sse.body.getReader().read()
assert.match(new TextDecoder().decode(value), /event: hello/)
controller.abort()
console.log('PASS SSE 经代理直通，首帧 hello')

/* ------------------------------------------------------------ 工具执行 */

const tasksResult = await tools.get('winsync_tasks').execute({}, {})
assert.equal(tasksResult.ok, true)
assert.equal(tasksResult.data.tasks.length, 1)
assert.equal(tasksResult.data.tasks[0].has_token, true)
assert.ok(!JSON.stringify(tasksResult).includes('p2-token'), '工具结果不能含明文密钥')

const statusResult = await tools.get('winsync_status').execute({ task: 'P2 冒烟' }, {})
assert.equal(statusResult.ok, true)
assert.equal(statusResult.data.task.id, taskId)
assert.equal(statusResult.data.runtime.running, false)

const activityResult = await tools.get('winsync_recent_activity').execute({ limit: 5 }, {})
assert.equal(activityResult.ok, true)
assert.deepEqual(activityResult.data.rows, [])

const missing = await tools.get('winsync_status').execute({ task: '不存在的任务' }, {})
assert.equal(missing.ok, false)
assert.equal(missing.error.code, 'not-found')
console.log('PASS 4 个工具里 3 个只读工具按契约返回')

/* ------------------------------------------------------------ 卸载清理 */

fs.rmSync(root, { recursive: true, force: true })
server.close()
const handshake = path.join(stateDir, 'daemon.json')
const pid = JSON.parse(fs.readFileSync(handshake, 'utf8')).pid
for (const { dispose } of effects.reverse()) {
  await dispose?.()
}
await new Promise((resolve) => setTimeout(resolve, 800))
assert.ok(!fs.existsSync(handshake), '卸载后 daemon.json 应该被删掉')
let alive = true
try {
  process.kill(pid, 0)
} catch {
  alive = false
}
assert.equal(alive, false, `卸载后 daemon ${pid} 仍在运行`)
fs.rmSync(stateDir, { recursive: true, force: true })
console.log('PASS 卸载后 daemon 停止、握手文件清理')

console.log('P2 smoke: 全部通过')
