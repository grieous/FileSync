/**
 * daemon 冒烟测试：对着一台**已经在跑**的 daemon 走一遍 HTTP 契约。
 *
 * 用法：
 *   node scripts/smoke.mjs [state-dir]
 *
 * 默认读 %LOCALAPPDATA%\dsh-winsync\daemon.json。会创建一个临时任务、改一次配置、
 * 列一次目录、拉一次 CSV、连一次 SSE，最后把任务删掉——只碰临时目录，不碰真实数据。
 */

import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const stateDir = process.argv[2] ?? path.join(process.env.LOCALAPPDATA ?? os.homedir(), 'dsh-winsync')
const handshake = path.join(stateDir, 'daemon.json')

assert.ok(fs.existsSync(handshake), `找不到握手文件：${handshake}（daemon 没起来？）`)
const { port, token } = JSON.parse(fs.readFileSync(handshake, 'utf8'))
const base = `http://127.0.0.1:${port}`

const call = async (pathname, init = {}) => {
  const response = await fetch(`${base}${pathname}`, {
    ...init,
    headers: {
      authorization: `Bearer ${token}`,
      ...(init.body ? { 'content-type': 'application/json' } : {}),
      ...init.headers,
    },
  })
  const text = await response.text()
  let body
  try {
    body = JSON.parse(text)
  } catch {
    body = text
  }
  return { status: response.status, body, contentType: response.headers.get('content-type') }
}

const pass = (label) => console.log(`PASS ${label}`)

/* ---------------------------------------------------------------- 健康与鉴权 */

const health = await call('/health')
assert.equal(health.status, 200)
assert.equal(health.body.ok, true)
assert.ok(!('token' in health.body), 'health 不能泄露 token')
pass('GET /health 免鉴权且不含 token')

const noAuth = await fetch(`${base}/api/state`)
assert.equal(noAuth.status, 401)
pass('无 Bearer 访问 /api/state → 401')

const state = await call('/api/state')
assert.equal(state.status, 200)
assert.equal(state.body.ok ?? true, true)
assert.ok(Array.isArray(state.body.tasks))
pass(`GET /api/state 返回 ${state.body.tasks.length} 个任务`)

/* ---------------------------------------------------------------- 任务 CRUD */

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'wsd-smoke-'))
fs.mkdirSync(path.join(root, 'Docs'))
fs.mkdirSync(path.join(root, 'Pics'))

const created = await call('/api/tasks', {
  method: 'POST',
  body: JSON.stringify({
    server: 'ws://127.0.0.1:18443/ws',
    token: 'smoke-token',
    workspace: 'default',
    root,
  }),
})
assert.equal(created.status, 200, JSON.stringify(created.body))
const task = created.body.task
assert.ok(task.id)
assert.equal(task.display_name, path.basename(root))
assert.deepEqual(task.issues, [])
assert.equal(task.has_token, true)
pass(`POST /api/tasks 建出任务 ${task.id}`)

const patchedRoot = await call(`/api/tasks/${task.id}`, {
  method: 'PATCH',
  body: JSON.stringify({ root: path.join(root, 'Docs') }),
})
assert.equal(patchedRoot.status, 409, '换目录没带 force 必须被拦')
assert.equal(patchedRoot.body.error.code, 'root-changed')
pass('PATCH 换目录不带 force → 409 root-changed')

const patchedName = await call(`/api/tasks/${task.id}`, {
  method: 'PATCH',
  body: JSON.stringify({ name: '冒烟任务' }),
})
assert.equal(patchedName.status, 200)
assert.equal(patchedName.body.task.display_name, '冒烟任务')
pass('PATCH 改名生效')

/* ---------------------------------------------------------------- 全局设置 */

const before = state.body.settings
const patchedSettings = await call('/api/settings', {
  method: 'PATCH',
  body: JSON.stringify({ limit_up_kbps: 1234 }),
})
assert.equal(patchedSettings.status, 200, JSON.stringify(patchedSettings.body))
assert.equal(patchedSettings.body.settings.limit_up_kbps, 1234)
assert.equal(
  patchedSettings.body.settings.log_keep_days,
  before.log_keep_days,
  '没传的字段必须保持不变',
)
pass('PATCH /api/settings 只改传入字段')

const restored = await call('/api/settings', {
  method: 'PATCH',
  body: JSON.stringify({ limit_up_kbps: before.limit_up_kbps }),
})
assert.equal(restored.body.settings.limit_up_kbps, before.limit_up_kbps)
pass('PATCH /api/settings 可还原')

/* ---------------------------------------------------------------- 目录与导出 */

const listed = await call(`/api/fs/list?task=${task.id}`)
assert.equal(listed.status, 200)
assert.deepEqual(
  listed.body.dirs.map((d) => d.name),
  ['Docs', 'Pics'],
)
pass('GET /api/fs/list 只返回子目录')

const traversal = await call(`/api/fs/list?task=${task.id}&path=..`)
assert.equal(traversal.status, 400, '路径穿越必须被拒')
pass('GET /api/fs/list 拒绝 ..')

const csv = await call(`/api/export.csv?task=all`)
assert.equal(csv.status, 200)
assert.match(csv.contentType ?? '', /text\/csv/)
assert.ok(csv.body.startsWith('time,task,file,event,status,message\n'))
pass('GET /api/export.csv 表头与 exe 一致')

/* ---------------------------------------------------------------- SSE */

const controller = new AbortController()
const stream = await fetch(`${base}/api/events`, {
  headers: { authorization: `Bearer ${token}` },
  signal: controller.signal,
})
assert.equal(stream.status, 200)
const reader = stream.body.getReader()
const { value } = await reader.read()
const first = new TextDecoder().decode(value)
assert.match(first, /event: hello/)
controller.abort()
pass('GET /api/events 首帧是 hello')

/* ---------------------------------------------------------------- 清理 */

const removed = await call(`/api/tasks/${task.id}`, { method: 'DELETE' })
assert.equal(removed.status, 200)
fs.rmSync(root, { recursive: true, force: true })
pass('DELETE /api/tasks 清理完成')

console.log('daemon smoke: 全部通过')
