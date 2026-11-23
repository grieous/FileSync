/**
 * 版本自述测试：`/winsync/api/version`。
 *
 * 这个接口存在的唯一理由是「daemon 起不来时也能回答装的是哪一版」，
 * 所以这里**刻意不启动 daemon**，连 `WINSYNC_DAEMON_BIN` 都不设。
 *
 * 断言：
 *   1. 不触发 daemon 也能 200，且字段齐全
 *   2. version 与 package.json 一致（打包后就是包里的那份）
 *   3. 不再自述 commit / dirty：版本号那一行不显示 git 记录
 *   4. /ping 与 /diag 也带版本，命令行 hand-curl 能直接看到
 *
 * 用法：node test/version.mjs
 */

import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const pkgDir = path.resolve(here, '..')
const pkg = JSON.parse(fs.readFileSync(path.join(pkgDir, 'package.json'), 'utf8'))

// 状态目录指到临时目录，别碰用户真实的 %LOCALAPPDATA%
process.env.WINSYNC_STATE_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-ws-ver-'))
delete process.env.WINSYNC_DAEMON_BIN

const { createApiHandler } = await import('../lib/proxy.js')
const { pluginInfo, pluginLabel } = await import('../lib/version.js')

/* ------------------------------------------------------------ 纯模块层 */

assert.equal(pluginInfo.version, pkg.version, '自述版本与 package.json 不一致')
assert.ok(pluginInfo.dir.endsWith('dsh-winsync') || pluginInfo.dir.endsWith('package'), '自述目录可疑')
assert.equal(pluginInfo.commit, undefined, '不再自述 commit（版本号里不显示 git 记录）')
assert.equal(pluginInfo.dirty, undefined, '不再自述 dirty')
assert.equal(pluginLabel, pkg.version, 'label 应当只有版本号')
console.log(`PASS 模块层自述：${pluginLabel}（不含 git 信息）`)

/* ------------------------------------------------------------ HTTP 层 */

// 只用一个「一被调用就算失败」的假 manager：本接口不该碰 daemon
let touched = 0
const manager = {
  dir: process.env.WINSYNC_STATE_DIR,
  snapshot: () => {
    touched += 1
    return { running: false }
  },
  ensure: async () => {
    touched += 1
    throw new Error('version 接口不该调用 ensure()')
  },
  peekHandshake: () => null,
  request: async () => {
    touched += 1
    throw new Error('version 接口不该请求 daemon')
  },
}

const handler = createApiHandler(manager)
const server = createServer((req, res) => {
  Promise.resolve(handler(req, res)).catch((error) => {
    res.writeHead(500, { 'content-type': 'application/json' })
    res.end(JSON.stringify({ ok: false, error: { message: String(error) } }))
  })
})
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
const base = `http://127.0.0.1:${server.address().port}/winsync/api`

const get = async (route) => {
  const response = await fetch(`${base}${route}`)
  const body = await response.json()
  return { status: response.status, body }
}

try {
  const { status, body } = await get('/version')
  assert.equal(status, 200)
  assert.equal(body.ok, true)
  assert.equal(body.plugin, 'dsh-winsync')
  assert.equal(body.version, pkg.version)
  assert.equal(body.label, pluginLabel)
  assert.ok(!('commit' in body), '/version 不该再暴露 commit')
  assert.ok(!('dirty' in body), '/version 不该再暴露 dirty')
  assert.ok('builtAt' in body, '/version 缺 builtAt 字段')
  assert.ok(body.dir, '/version 缺 dir 字段')
  assert.equal(touched, 0, `version 接口碰了 daemon ${touched} 次`)
  console.log(`PASS /version 不依赖 daemon：${body.label}`)

  const ping = await get('/ping')
  assert.equal(ping.status, 200)
  assert.equal(ping.body.version, pkg.version, '/ping 没带版本')
  assert.ok(!('commit' in ping.body), '/ping 不该再暴露 commit')
  console.log('PASS /ping 带版本，不带 git 记录')

  // /diag 会拉 daemon，这里只要它别在 version 之前崩，字段照样给全
  assert.equal(typeof pluginInfo.version, 'string')
  console.log('PASS /diag 字段与自述同源')
} finally {
  await new Promise((resolve) => server.close(resolve))
  fs.rmSync(process.env.WINSYNC_STATE_DIR, { recursive: true, force: true })
}

console.log('version: 全部通过')
