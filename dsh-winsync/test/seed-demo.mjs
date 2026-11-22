/**
 * 往指定 daemon 灌一批多样化的任务配置，用于界面走查。
 *
 *   node test/seed-demo.mjs <stateDir> [--reset] [--sync]
 *
 * 任务刻意覆盖：长名称/长路径、三种同步模式、过滤规则、暂停态、不可达服务器（错误态）。
 * 只动该状态目录自己的 daemon，不碰 exe 的 %APPDATA%\WinSync。
 */
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'

const stateDir = process.argv[2]
const doReset = process.argv.includes('--reset')
const doSync = process.argv.includes('--sync')
if (!stateDir) {
  console.error('用法：node test/seed-demo.mjs <stateDir> [--reset] [--sync]')
  process.exit(1)
}

const handshakePath = path.join(stateDir, 'daemon.json')
if (!fs.existsSync(handshakePath)) {
  console.error(`没有握手文件：${handshakePath}（daemon 没起来？）`)
  process.exit(1)
}
const { port, token } = JSON.parse(fs.readFileSync(handshakePath, 'utf8'))
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

if (doReset) {
  const state = await call('/api/state')
  for (const task of state.body?.tasks ?? []) {
    await call(`/api/tasks/${task.id}`, { method: 'DELETE' })
  }
  console.log(`清空 ${state.body?.tasks?.length ?? 0} 个旧任务`)
}

const demo = path.join(os.tmpdir(), 'winsync-demo')
const relay = 'ws://127.0.0.1:18443/ws'
const relayToken = 'demo-token-2026'
const noFilter = { ignore_dotfiles: false, ignore_extensions: [], size_min: null, size_max: null }

// 每种任务对应一个真实场景：有对端在跑的（office / code）会同步成功，其余保持干净的空态
const tasks = [
  {
    name: '工作文档',
    server: relay,
    token: relayToken,
    workspace: 'office',
    root: path.join(demo, '工作文档'),
    mode: 'bidirectional',
    filter: noFilter,
    sync: true,
  },
  {
    name: '设计素材库（团队共享）',
    server: relay,
    token: relayToken,
    workspace: 'design',
    root: path.join(demo, '设计素材库'),
    mode: 'upload_only',
    filter: { ignore_dotfiles: true, ignore_extensions: ['tmp', 'psd', 'bak'], size_min: null, size_max: null },
    paused: true,
  },
  {
    name: '照片归档',
    server: relay,
    token: relayToken,
    workspace: 'photos',
    root: path.join(demo, '照片归档'),
    mode: 'download_only',
    filter: { ignore_dotfiles: false, ignore_extensions: [], size_min: 0, size_max: 52428800 },
    paused: true,
  },
  {
    // 长名称 + 长路径，专门压标题与副标题的截断
    name: '代码仓库快照 — 每周三凌晨自动备份到团队共享目录',
    server: relay,
    token: relayToken,
    workspace: 'code',
    root: path.join(demo, 'code'),
    mode: 'bidirectional',
    filter: noFilter,
    sync: true,
  },
  {
    // 不可达服务器：用来检查错误态文案与颜色
    name: '旧项目（服务器已下线）',
    server: 'ws://127.0.0.1:9/ws',
    token: 'stale-token',
    workspace: 'legacy',
    root: path.join(demo, '工作文档'),
    mode: 'bidirectional',
    filter: noFilter,
    sync: true,
  },
]

const created = []
for (const task of tasks) {
  const { sync, ...payload } = task
  const reply = await call('/api/tasks', { method: 'POST', body: JSON.stringify(payload) })
  if (reply.status !== 200) {
    console.error(`建任务失败 ${task.name}: ${reply.status} ${JSON.stringify(reply.body)}`)
    continue
  }
  created.push({ view: reply.body.task, sync: Boolean(sync) && !task.paused })
  console.log(`created ${reply.body.task.id}  ${task.name}${task.paused ? '（暂停）' : ''}`)
}

if (doSync) {
  for (const { view, sync } of created) {
    if (!sync) continue
    const reply = await call(`/api/tasks/${view.id}/sync`, { method: 'POST', body: '{}' })
    console.log(`sync    ${view.id} → ${reply.status} ${JSON.stringify(reply.body)}`)
  }
}

const state = await call('/api/state')
console.log(`\n共 ${state.body.tasks.length} 个任务`)
for (const t of state.body.tasks.slice(-tasks.length)) {
  const r = t.runtime ?? {}
  console.log(
    `  ${t.id}  ${t.display_name}  ${t.mode}  paused=${t.paused}  running=${r.running}  err=${r.last_error ?? '-'}`,
  )
}
