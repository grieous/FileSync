/**
 * P4 端到端实测：真实 relay + 两个真实对端，按计划里 4.1 / 4.2 / 4.3 逐条断言。
 *
 *   node sync/tools/e2e.mjs                # 全部跑
 *   node sync/tools/e2e.mjs --only fuse    # 只跑某几个（名字子串匹配）
 *   node sync/tools/e2e.mjs --bin release   # 用 release 产物（默认 debug）
 *   node sync/tools/e2e.mjs --keep          # 保留 .tmp/p4，便于事后翻文件
 *
 * 设计取舍：
 * - 不用 mock、不走库函数直调，全部起真实进程——两端的 WS 会话、帧序、
 *   文件落地、状态库都要真跑，否则测不到「一方推完另一方还在接收」这类时序问题。
 * - 每个场景一个独立 room + 独立状态目录 + 独立目录树，互不污染；
 *   场景之间不共享 base 快照，失败时也不会把别的场景带崩。
 * - 目录在 `.tmp/p4/`（已被 .gitignore 忽略），跑完即删。
 */
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import net from 'node:net'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const repo = path.resolve(here, '../..')
const args = process.argv.slice(2)
const binKind = valueOf('--bin') ?? 'debug'
const only = args.includes('--only') ? valueOf('--only') : null
const keep = args.includes('--keep')
const exe = (name) => path.join(repo, 'sync', 'target', binKind, `${name}.exe`)
const WORK = path.join(repo, '.tmp', 'p4')

const CLI = exe('sync-client')
const DAEMON = exe('sync-daemon')
const RELAY = exe('sync-server')
const TOKEN = 'p4-token'
const NO_FILTER = { ignore_dotfiles: false, ignore_extensions: [], size_min: null, size_max: null }

/* ------------------------------------------------------------------ 小工具 */

function valueOf(flag) {
  const at = args.indexOf(flag)
  return at >= 0 ? args[at + 1] : undefined
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

/** 跑一个进程，拿退出码和合并后的输出；超时直接杀掉并标记。 */
function run(file, argv, { env = {}, timeout = 90_000, cwd = repo } = {}) {
  return new Promise((resolve) => {
    const child = spawn(file, argv, {
      cwd,
      env: { ...process.env, ...env },
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let out = ''
    let timedOut = false
    const timer = setTimeout(() => {
      timedOut = true
      child.kill()
    }, timeout)
    child.stdout.on('data', (d) => (out += d))
    child.stderr.on('data', (d) => (out += d))
    child.on('close', (code) => {
      clearTimeout(timer)
      resolve({ code, out, timedOut })
    })
  })
}

async function freePort() {
  const server = net.createServer()
  await new Promise((ok) => server.listen(0, '127.0.0.1', ok))
  const { port } = server.address()
  await new Promise((ok) => server.close(ok))
  return port
}

/** 递归列出目录树：相对路径 → 内容哈希（目录用 `<dir>` 占位）。 */
function tree(dir) {
  const out = {}
  const walk = (current, prefix) => {
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const rel = prefix ? `${prefix}/${entry.name}` : entry.name
      const full = path.join(current, entry.name)
      if (entry.isDirectory()) {
        out[rel] = '<dir>'
        walk(full, rel)
      } else {
        out[rel] = crypto.createHash('sha1').update(fs.readFileSync(full)).digest('hex').slice(0, 12)
      }
    }
  }
  walk(dir, '')
  return out
}

function writeTree(dir, files) {
  fs.mkdirSync(dir, { recursive: true })
  for (const [rel, body] of Object.entries(files)) {
    const full = path.join(dir, rel)
    fs.mkdirSync(path.dirname(full), { recursive: true })
    fs.writeFileSync(full, body)
  }
}

function writeConfig(stateDir, task) {
  fs.mkdirSync(stateDir, { recursive: true })
  const config = {
    version: 2,
    tasks: [
      {
        id: task.id ?? 'p4',
        name: task.name ?? 'P4',
        server: task.server,
        token: TOKEN,
        workspace: task.workspace,
        root: task.root,
        mode: task.mode ?? 'bidirectional',
        filter: task.filter ?? NO_FILTER,
        paused: false,
      },
    ],
  }
  fs.writeFileSync(path.join(stateDir, 'config.json'), JSON.stringify(config, null, 2))
}

function cliSync(stateDir, extra = []) {
  return run(CLI, [...extra, '--wait', '25', 'sync'], { env: { WINSYNC_STATE_DIR: stateDir } })
}

/** 两端同时跑一次同步——会话需要两边同时在线上才能配对。 */
async function syncBoth(left, right, { leftArgs = [], rightArgs = [] } = {}) {
  const [a, b] = await Promise.all([cliSync(left.state, leftArgs), cliSync(right.state, rightArgs)])
  if (process.env.E2E_VERBOSE) {
    console.log(`  --- ${left.name} (exit ${a.code})\n${a.out}`)
    console.log(`  --- ${right.name} (exit ${b.code})\n${b.out}`)
  }
  return { a, b }
}

/* ------------------------------------------------------------------ 断言 */

let failures = 0
let checks = 0
const current = { name: '?' }

function ok(condition, message, detail) {
  checks += 1
  if (condition) {
    console.log(`  PASS ${message}`)
    return true
  }
  failures += 1
  console.log(`  FAIL ${message}`)
  if (detail !== undefined) console.log(`       ${String(detail).replace(/\n/g, '\n       ')}`)
  return false
}

function eq(actual, expected, message) {
  const same = JSON.stringify(actual) === JSON.stringify(expected)
  return ok(same, message, same ? undefined : `实际 ${JSON.stringify(actual)}\n期望 ${JSON.stringify(expected)}`)
}

function contains(text, needle, message) {
  return ok(String(text).includes(needle), message, String(text).slice(-600))
}

/* ------------------------------------------------------------------ 场景骨架 */

/** 一个对端：状态目录 + 同步目录 + 配置，三者一一对应。 */
function makePeer(name, { server, workspace, mode, filter, root }) {
  const base = path.join(WORK, current.name, name)
  const peer = {
    name,
    state: path.join(base, 'state'),
    root: root ?? path.join(base, 'files'),
  }
  fs.rmSync(base, { recursive: true, force: true })
  fs.mkdirSync(peer.root, { recursive: true })
  writeConfig(peer.state, { server, workspace, mode, filter, root: peer.root })
  return peer
}

/** 不含 state/ 的同步目录快照——状态库不该出现在同步结果里。 */
const filesOf = (peer) => tree(peer.root)

const scenarios = []
const scenario = (name, fn) => scenarios.push({ name, fn })

/**
 * 起一个真 daemon，等它写下握手文件，返回带 Bearer 的 api()。
 * APPDATA 一定传伪造目录：overlap 会去读 `%APPDATA%\WinSync\config.json`，
 * 测试绝不能把用户真实的 exe 配置读成自己的边界条件（更不能写）。
 */
async function startDaemon({ state, appdata }) {
  const child = spawn(DAEMON, [], {
    env: { ...process.env, WINSYNC_STATE_DIR: state, APPDATA: appdata, RUST_LOG: 'warn' },
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  let out = ''
  child.stdout.on('data', (d) => (out += d))
  child.stderr.on('data', (d) => (out += d))

  const handshake = path.join(state, 'daemon.json')
  for (let i = 0; i < 80 && !fs.existsSync(handshake); i += 1) await sleep(100)
  if (!fs.existsSync(handshake)) {
    child.kill()
    return { api: null, out: () => out, stop: () => {} }
  }
  const { port, token } = JSON.parse(fs.readFileSync(handshake, 'utf8'))
  const api = async (pathname, init = {}) => {
    const response = await fetch(`http://127.0.0.1:${port}${pathname}`, {
      ...init,
      headers: {
        authorization: `Bearer ${token}`,
        ...(init.body ? { 'content-type': 'application/json' } : {}),
      },
    })
    const text = await response.text()
    let body = null
    try {
      body = JSON.parse(text)
    } catch {
      body = text
    }
    return { status: response.status, body, text }
  }
  return {
    api,
    out: () => out,
    async stop() {
      child.kill()
      await sleep(300)
    },
  }
}

/* ------------------------------------------------------------------ 4.1 双向 */

scenario('bidir', async (ctx) => {
  const left = makePeer('left', ctx)
  const right = makePeer('right', ctx)

  writeTree(left.root, {
    'readme.md': 'hello',
    'docs/a.txt': 'aaa',
    'docs/deep/b.txt': 'bbb',
  })

  let round = await syncBoth(left, right)
  ok(round.a.code === 0 && round.b.code === 0, '首轮同步两端都成功', round.a.out + round.b.out)
  eq(filesOf(right), filesOf(left), '首轮：右端拿到完整目录树')

  // 右端改动 + 新增，左端删一个：验证推、拉、删除传播同时成立
  fs.appendFileSync(path.join(right.root, 'readme.md'), ' + right')
  writeTree(right.root, { 'docs/new.txt': 'from right' })
  fs.rmSync(path.join(left.root, 'docs/a.txt'))

  round = await syncBoth(left, right)
  ok(round.a.code === 0 && round.b.code === 0, '第二轮同步成功', round.a.out + round.b.out)
  eq(filesOf(right), filesOf(left), '第二轮：两端完全一致（含删除传播）')
  ok(filesOf(left)['docs/a.txt'] === undefined, '左端删除的文件在右端也消失了')
  ok(filesOf(left)['docs/new.txt'] !== undefined, '右端新增的文件到了左端')

  // 再跑一次应该是零传输
  round = await syncBoth(left, right)
  contains(round.a.out, '两端已一致', '第三轮：左端判定已一致（零传输）')
  contains(round.b.out, '两端已一致', '第三轮：右端判定已一致（零传输）')
})

/* ------------------------------------------------------------------ 4.1 单向 */

scenario('upload', async (ctx) => {
  const left = makePeer('sender', { ...ctx, mode: 'upload_only' })
  const right = makePeer('receiver', { ...ctx, mode: 'download_only' })

  writeTree(left.root, { 'a.txt': 'A' })
  writeTree(right.root, { 'existing.txt': 'R' })

  let round = await syncBoth(left, right)
  ok(round.a.code === 0 && round.b.code === 0, '仅上传 ⇄ 仅下载：首轮成功', round.a.out + round.b.out)
  ok(filesOf(right)['a.txt'] !== undefined, '发送端的文件到了接收端')
  ok(filesOf(left)['existing.txt'] === undefined, '接收端原有文件没有反向流回发送端')

  // 接收端本地新增：单向配置下不该被推出去
  writeTree(right.root, { 'local-only.txt': 'keep local' })
  round = await syncBoth(left, right)
  ok(round.b.code === 0, '接收端本地改动不导致失败', round.b.out)
  ok(filesOf(left)['local-only.txt'] === undefined, '仅下载端的新文件不会被推给对端')
})

scenario('download', async (ctx) => {
  // 角色反过来：本机只下载，对端只上传
  const left = makePeer('receiver', { ...ctx, mode: 'download_only' })
  const right = makePeer('sender', { ...ctx, mode: 'upload_only' })

  writeTree(right.root, { 'from-right.txt': 'R' })
  writeTree(left.root, { 'untouched.txt': 'L' })

  const round = await syncBoth(left, right)
  ok(round.a.code === 0 && round.b.code === 0, '仅下载 ⇄ 仅上传：同步成功', round.a.out + round.b.out)
  ok(filesOf(left)['from-right.txt'] !== undefined, '对端文件被拉到本机')
  ok(filesOf(right)['untouched.txt'] === undefined, '本机文件没有反向推给对端')
})

/* ------------------------------------------------------------------ 冲突 */

scenario('conflict', async (ctx) => {
  const left = makePeer('left', ctx)
  const right = makePeer('right', ctx)

  writeTree(left.root, { 'note.txt': 'v1' })
  await syncBoth(left, right)

  // 同一路径两端都改，右端更晚 → 右端胜出，左端旧版本留成冲突副本
  fs.writeFileSync(path.join(left.root, 'note.txt'), 'left-edit')
  await sleep(1100)
  fs.writeFileSync(path.join(right.root, 'note.txt'), 'right-edit-newer')

  const round = await syncBoth(left, right)
  ok(round.a.code === 0 && round.b.code === 0, '冲突轮次两端都成功（冲突不报错）', round.a.out + round.b.out)

  const leftFiles = Object.keys(filesOf(left))
  const rightFiles = Object.keys(filesOf(right))
  const copies = (list) => list.filter((n) => /\.conflict-[0-9a-f]{8}-\d{8}-\d{6}/.test(n))
  ok(copies(leftFiles).length + copies(rightFiles).length === 1, '败方留下一份冲突副本，且只有一份',
    `left=${leftFiles.join(',')} right=${rightFiles.join(',')}`)
  ok(fs.readFileSync(path.join(right.root, 'note.txt'), 'utf8') === 'right-edit-newer',
    '两端最终都是较新版本（右端的修改）')
  ok(!copies(leftFiles).concat(copies(rightFiles)).some((n) => rightFiles.includes(n)),
    '冲突副本不参与同步（没有两端各一份地增殖）')
  const propagated = (copies(leftFiles).length ? copies(leftFiles) : copies(rightFiles))[0]
  ok(!(rightFiles.includes(propagated) && leftFiles.includes(propagated)), '冲突副本没有被传到对端')
})

/* ------------------------------------------------------------------ 4.2 过滤不一致 */

scenario('filter-mismatch', async (ctx) => {
  const left = makePeer('left', {
    ...ctx,
    filter: { ignore_dotfiles: true, ignore_extensions: ['tmp'], size_min: null, size_max: null },
  })
  const right = makePeer('right', ctx)

  writeTree(left.root, { 'keep.txt': 'k', 'cache.tmp': 'ignored' })
  writeTree(right.root, { 'other.txt': 'o' })

  const round = await syncBoth(left, right)
  ok(round.a.code !== 0, '过滤不一致：左端中止（非零退出）')
  ok(round.b.code !== 0, '过滤不一致：右端中止（非零退出）')
  contains(round.a.out, '过滤规则不一致', '左端报出「过滤规则不一致」')
  contains(round.b.out, '过滤规则不一致', '右端报出「过滤规则不一致」')

  const leftFiles = filesOf(left)
  ok(leftFiles['keep.txt'] !== undefined && leftFiles['cache.tmp'] !== undefined, '左端文件零改动',
    JSON.stringify(leftFiles))
  eq(Object.keys(filesOf(right)), ['other.txt'], '右端文件零改动（没收到 .tmp，也没丢 other.txt）')
})

/* ------------------------------------------------------------------ 4.2 删除熔断 */

scenario('delete-fuse', async (ctx) => {
  const left = makePeer('left', ctx)
  const right = makePeer('right', ctx)

  const many = {}
  for (let i = 0; i < 25; i += 1) many[`bulk/f${String(i).padStart(2, '0')}.txt`] = `body ${i}`
  writeTree(left.root, many)
  let round = await syncBoth(left, right)
  ok(round.a.code === 0 && round.b.code === 0, '铺 25 个文件后两端同步成功', round.a.out + round.b.out)
  eq(Object.keys(filesOf(right)).length, 26, '右端拿到 25 个文件 + 1 个目录')

  // 一端整目录清空：超过「>20 项或 >30%」的阈值，必须中止而不是照删
  fs.rmSync(path.join(left.root, 'bulk/f00.txt'))
  round = await syncBoth(left, right)
  ok(round.a.code === 0 && round.b.code === 0, '只删 1 项（低于阈值）：正常放行', round.a.out + round.b.out)
  ok(filesOf(right)['bulk/f00.txt'] === undefined, '单项删除正常传播到对端')

  fs.rmSync(path.join(left.root, 'bulk'), { recursive: true })
  round = await syncBoth(left, right)
  ok(round.a.code !== 0, '一次删 24 项：左端中止')
  ok(round.b.code !== 0, '一次删 24 项：右端中止')
  contains(round.a.out, '已中止', '左端给出中止说明')
  ok(/将删除 \d+ 个文件/.test(round.a.out + round.b.out), '提示里点明将删除多少文件', round.a.out)
  eq(Object.keys(filesOf(right)).filter((n) => n.startsWith('bulk/')).length, 24, '右端 24 个文件一个没少')
})

/* ------------------------------------------------------------------ 4.2 换目录 */

scenario('root-change', async (ctx) => {
  const left = makePeer('left', ctx)
  const right = makePeer('right', ctx)
  const moved = path.join(WORK, current.name, 'left', 'moved')

  writeTree(left.root, { 'a.txt': 'A' })
  await syncBoth(left, right)

  // 等价于用户在界面里把目录改到另一处：配置里的 root 变了
  fs.mkdirSync(moved, { recursive: true })
  writeTree(moved, { 'b.txt': 'B' })
  writeConfig(left.state, { ...ctx, root: moved })
  left.root = moved

  const blocked = await cliSync(left.state)
  ok(blocked.code !== 0, '换目录未确认：拒绝执行')
  contains(blocked.out, '同步目录已变更', '提示里说明目录变了')
  contains(blocked.out, '--force', '提示里告诉用户怎么确认')
  ok(!fs.existsSync(path.join(moved, 'a.txt')), '被拒绝时新目录仍为空（没有乱写）')

  const forced = await syncBoth(left, right, { leftArgs: ['--force'] })
  ok(forced.a.code === 0, '带 --force 后允许换目录', forced.a.out)
  eq(filesOf(left), filesOf(right), '换目录后两端重新建立共识并一致')
  // 语义说明：新目录里没有 a.txt，对端会把它当「本机已删除」。
  // 单文件规模下熔断不介入，这是目录不一致时唯一的实际风险，README 已写明。
  ok(filesOf(left)['b.txt'] !== undefined, '新目录的文件已推到对端')
})

/* ------------------------------------------------------------------ 4.3 daemon ↔ CLI */

scenario('daemon-cli', async (ctx) => {
  const peer = makePeer('cli-peer', ctx)
  const daemonRoot = path.join(WORK, current.name, 'daemon-files')
  const state = path.join(WORK, current.name, 'daemon-state')
  const fakeAppData = path.join(WORK, current.name, 'appdata')
  fs.rmSync(daemonRoot, { recursive: true, force: true })
  fs.rmSync(state, { recursive: true, force: true })
  fs.mkdirSync(daemonRoot, { recursive: true })
  fs.mkdirSync(fakeAppData, { recursive: true })

  const child = await startDaemon({ state, appdata: fakeAppData })

  try {
    if (!child.api) {
      ok(false, 'daemon 起来并写下握手文件', child.out())
      return
    }
    const api = child.api

    const created = await api('/api/tasks', {
      method: 'POST',
      body: JSON.stringify({
        name: 'P4 互操作',
        server: ctx.server,
        token: TOKEN,
        workspace: ctx.workspace,
        root: daemonRoot,
        mode: 'bidirectional',
        filter: NO_FILTER,
      }),
    })
    ok(created.status === 200, 'daemon 建任务成功', JSON.stringify(created.body))
    const taskId = created.body?.task?.id
    if (!taskId) return

    writeTree(peer.root, { 'from-peer.txt': 'peer', 'dir/nested.txt': 'nested' })
    writeTree(daemonRoot, { '带,逗号.txt': 'comma', 'from-daemon.txt': 'daemon' })

    // daemon 触发同步 + 对端 CLI 同步，两边会话配对
    const [syncRes, peerRes] = await Promise.all([
      api(`/api/tasks/${taskId}/sync`, { method: 'POST' }),
      cliSync(peer.state),
    ])
    ok(syncRes.status === 200, 'daemon 接受「立即同步」', JSON.stringify(syncRes.body))
    ok(peerRes.code === 0, 'CLI 对端同步成功', peerRes.out)

    // 等 daemon 侧会话结束（running=false 且 last_time 有值）
    let view = null
    for (let i = 0; i < 100; i += 1) {
      const stateRes = await api('/api/state')
      view = stateRes.body?.tasks?.find((t) => t.id === taskId)
      if (view && !view.runtime.running && (view.runtime.last || view.runtime.last_error)) break
      await sleep(200)
    }
    ok(!view?.runtime?.last_error, 'daemon 侧同步没有报错', JSON.stringify(view?.runtime?.last_error))
    ok(view?.runtime?.last?.pushed > 0 || view?.runtime?.last?.pulled > 0, 'daemon 侧报告有实际传输',
      JSON.stringify(view?.runtime?.last))

    const daemonFiles = tree(daemonRoot)
    const peerFiles = tree(peer.root)
    ok(daemonFiles['from-peer.txt'] !== undefined, 'daemon 目录拿到了对端文件')
    ok(peerFiles['from-daemon.txt'] !== undefined, '对端拿到了 daemon 的文件')
    eq(Object.keys(daemonFiles).sort(), Object.keys(peerFiles).sort(), '两侧目录树一致')

    // CSV：表头与 exe 一致，含逗号的文件名必须加引号
    const csv = await api('/api/export.csv')
    ok(csv.status === 200, 'CSV 导出返回 200')
    ok(String(csv.body).startsWith('time,task,file,event,status,message'), 'CSV 表头与 exe 一致',
      String(csv.body).slice(0, 120))
    contains(String(csv.body), '"带,逗号.txt"', 'CSV 对含逗号的文件名做了转义')
    contains(String(csv.body), 'from-peer.txt', 'CSV 里有本轮传输记录')

    // 设置 PATCH：只改传入字段，其它保持
    const before = await api('/api/state')
    const patched = await api('/api/settings', {
      method: 'PATCH',
      body: JSON.stringify({ limit_up_kbps: 64 }),
    })
    ok(patched.status === 200, 'PATCH /api/settings 成功', JSON.stringify(patched.body))
    const after = await api('/api/state')
    eq(after.body?.settings?.limit_up_kbps, 64, '限速字段已更新')
    eq(after.body?.settings?.log_keep_days, before.body?.settings?.log_keep_days, '未传的字段保持原值')
    eq(after.body?.tasks?.length, before.body?.tasks?.length, 'PATCH 设置不影响任务列表')
  } finally {
    await child.stop()
  }
})

/* ------------------------------------------------------------------ 限速 */

scenario('rate-limit', async (ctx) => {
  const peer = makePeer('cli-peer', { ...ctx, mode: 'upload_only' })
  const daemonRoot = path.join(WORK, current.name, 'daemon-files')
  const state = path.join(WORK, current.name, 'daemon-state')
  const appdata = path.join(WORK, current.name, 'appdata')
  fs.rmSync(daemonRoot, { recursive: true, force: true })
  fs.rmSync(state, { recursive: true, force: true })
  fs.mkdirSync(daemonRoot, { recursive: true })
  fs.mkdirSync(appdata, { recursive: true })

  const daemon = await startDaemon({ state, appdata })
  try {
    if (!daemon.api) {
      ok(false, '限速场景：daemon 启动', daemon.out())
      return
    }
    const api = daemon.api
    // 关掉自动同步：计时必须只包含我们手动触发的那一次会话
    await api('/api/auto', { method: 'POST', body: JSON.stringify({ on: false }) })
    await api('/api/settings', { method: 'PATCH', body: JSON.stringify({ limit_down_kbps: 256 }) })

    const created = await api('/api/tasks', {
      method: 'POST',
      body: JSON.stringify({
        name: '限速验证',
        server: ctx.server,
        token: TOKEN,
        workspace: ctx.workspace,
        root: daemonRoot,
        mode: 'bidirectional',
        filter: NO_FILTER,
      }),
    })
    const taskId = created.body?.task?.id
    if (!taskId) {
      ok(false, '限速场景：建任务', JSON.stringify(created.body))
      return
    }

    const payload = crypto.randomBytes(1536 * 1024) // 1.5 MB
    const waitIdle = async () => {
      for (let i = 0; i < 300; i += 1) {
        const snapshot = await api('/api/state')
        const view = snapshot.body?.tasks?.find((t) => t.id === taskId)
        if (view && !view.runtime.running && (view.runtime.last || view.runtime.last_error)) return view
        await sleep(100)
      }
      return null
    }

    writeTree(peer.root, { 'big.bin': payload })
    const startedSlow = Date.now()
    const [, peerSlow] = await Promise.all([
      api(`/api/tasks/${taskId}/sync`, { method: 'POST' }),
      cliSync(peer.state),
    ])
    const slowView = await waitIdle()
    const slowMs = Date.now() - startedSlow
    ok(peerSlow.code === 0, '限速轮：对端上传完成', peerSlow.out)
    ok(!slowView?.runtime?.last_error, '限速轮：daemon 侧没有报错', JSON.stringify(slowView?.runtime?.last_error))
    ok(filesOf({ root: daemonRoot })['big.bin'] !== undefined, '限速轮：1.5 MB 文件已落地')
    // 1.5 MB / 256 KB/s ≈ 6.1s；给足抖动余量，只断言「明显被限住了」
    ok(slowMs > 3000, `限速生效：1.5 MB 用了 ${slowMs}ms（256 KB/s 理论 ≈ 6.1s）`)

    // 解除限速后同样大小应该快一个数量级
    await api('/api/settings', { method: 'PATCH', body: JSON.stringify({ limit_down_kbps: 0 }) })
    writeTree(peer.root, { 'big2.bin': payload })
    const startedFast = Date.now()
    const [, peerFast] = await Promise.all([
      api(`/api/tasks/${taskId}/sync`, { method: 'POST' }),
      cliSync(peer.state),
    ])
    const fastView = await waitIdle()
    const fastMs = Date.now() - startedFast
    ok(peerFast.code === 0, '解除限速轮：对端上传完成', peerFast.out)
    ok(!fastView?.runtime?.last_error, '解除限速轮：没有报错', JSON.stringify(fastView?.runtime?.last_error))
    ok(filesOf({ root: daemonRoot })['big2.bin'] !== undefined, '解除限速轮：第二个大文件已落地')
    ok(fastMs < slowMs / 2, `解除限速后明显变快：${fastMs}ms vs ${slowMs}ms`)
  } finally {
    await daemon.stop()
  }
})

/* ------------------------------------------------------------------ 4.3 隔离 */

scenario('isolation', async (ctx) => {
  const state = path.join(WORK, current.name, 'state')
  const fakeAppData = path.join(WORK, current.name, 'appdata')
  const exeHome = path.join(fakeAppData, 'WinSync')
  const sharedRoot = path.join(WORK, current.name, 'shared')
  fs.rmSync(state, { recursive: true, force: true })
  fs.rmSync(fakeAppData, { recursive: true, force: true })
  fs.mkdirSync(sharedRoot, { recursive: true })

  // 伪造 exe 的配置：已经有一个任务占了 sharedRoot
  fs.mkdirSync(exeHome, { recursive: true })
  const exeConfig = path.join(exeHome, 'config.json')
  fs.writeFileSync(
    exeConfig,
    JSON.stringify({
      version: 2,
      tasks: [
        {
          id: 'exe1',
          name: 'exe 自己的任务',
          server: 'ws://127.0.0.1:1/ws',
          token: 'exe',
          workspace: 'exe',
          root: sharedRoot,
          mode: 'bidirectional',
        },
      ],
    }),
  )
  const exeSnapshot = fs.readFileSync(exeConfig, 'utf8')
  const exeMtime = fs.statSync(exeConfig).mtimeMs

  const child = await startDaemon({ state, appdata: fakeAppData })

  try {
    if (!child.api) {
      ok(false, '隔离场景：daemon 启动', child.out())
      return
    }
    const api = child.api

    const clash = await api('/api/tasks', {
      method: 'POST',
      body: JSON.stringify({
        name: '抢 exe 的目录',
        server: ctx.server,
        token: TOKEN,
        workspace: ctx.workspace,
        root: sharedRoot,
        mode: 'bidirectional',
        filter: NO_FILTER,
      }),
    })
    ok(clash.status === 409, 'exe 已同步的目录：daemon 拒绝创建（409）', JSON.stringify(clash.body))
    eq(clash.body?.error?.code, 'root-conflict', '错误码是 root-conflict')
    contains(clash.body?.error?.message ?? '', 'exe 自己的任务', '错误信息点名了 exe 的任务')

    // 父目录包含也应被拦：daemon 任务 root 是 exe 任务的上级
    const parentClash = await api('/api/tasks', {
      method: 'POST',
      body: JSON.stringify({
        name: '父目录',
        server: ctx.server,
        token: TOKEN,
        workspace: ctx.workspace,
        root: path.dirname(sharedRoot),
        mode: 'bidirectional',
        filter: NO_FILTER,
      }),
    })
    ok(parentClash.status === 409, '父目录重叠也被拒绝（409）', JSON.stringify(parentClash.body))

    // 自己的目录可以建，且只在 dsh 状态树里写文件
    const own = path.join(WORK, current.name, 'own')
    fs.mkdirSync(own, { recursive: true })
    const created = await api('/api/tasks', {
      method: 'POST',
      body: JSON.stringify({
        name: '自己的目录',
        server: ctx.server,
        token: TOKEN,
        workspace: `${ctx.workspace}-own`,
        root: own,
        mode: 'bidirectional',
        filter: NO_FILTER,
      }),
    })
    ok(created.status === 200, '不重叠的目录可以建任务', JSON.stringify(created.body))
    ok(fs.existsSync(path.join(state, 'config.json')), 'daemon 把配置写在自己的状态树里')
    ok(fs.readFileSync(exeConfig, 'utf8') === exeSnapshot, 'exe 的 config.json 内容一字未改')
    ok(fs.statSync(exeConfig).mtimeMs === exeMtime, 'exe 的 config.json 连 mtime 都没动')
    ok(!fs.existsSync(path.join(exeHome, 'tasks')), 'exe 的状态目录里没有多出文件')
  } finally {
    await child.stop()
  }
})

/* ------------------------------------------------------------------ 主流程 */

async function main() {
  for (const file of [CLI, DAEMON, RELAY]) {
    if (!fs.existsSync(file)) {
      console.error(`缺少二进制：${file}\n先跑 cargo build --workspace（或 --bin release 配 cargo build --release）`)
      process.exit(2)
    }
  }

  const selected = only ? scenarios.filter((s) => s.name.includes(only)) : scenarios
  if (selected.length === 0) {
    console.error(`没有匹配的场景：${only}；可选 ${scenarios.map((s) => s.name).join(' / ')}`)
    process.exit(2)
  }
  if (!keep) fs.rmSync(WORK, { recursive: true, force: true })
  fs.mkdirSync(WORK, { recursive: true })

  const port = await freePort()
  const server = `ws://127.0.0.1:${port}/ws`
  const relay = spawn(RELAY, [], {
    env: { ...process.env, SYNC_TOKEN: TOKEN, SYNC_ADDR: `127.0.0.1:${port}`, RUST_LOG: 'warn' },
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  let relayOut = ''
  relay.stdout.on('data', (d) => (relayOut += d))
  relay.stderr.on('data', (d) => (relayOut += d))

  // 等端口真的能连上再开跑
  for (let i = 0; i < 60; i += 1) {
    const reachable = await new Promise((resolve) => {
      const probe = net.connect(port, '127.0.0.1')
      probe.once('connect', () => {
        probe.destroy()
        resolve(true)
      })
      probe.once('error', () => resolve(false))
    })
    if (reachable) break
    await sleep(100)
  }

  console.log(`relay  ws://127.0.0.1:${port}/ws   bin=${binKind}`)
  try {
    for (const item of selected) {
      current.name = item.name
      const workspace = `p4-${item.name}`
      const startedAt = Date.now()
      console.log(`\n[${item.name}]`)
      await item.fn({ server, workspace })
      console.log(`  —— ${Date.now() - startedAt}ms`)
    }
  } finally {
    relay.kill()
  }

  console.log(`\n结果：${checks - failures}/${checks} 通过`)
  if (failures > 0) {
    console.log(`relay 输出：\n${relayOut.slice(-1500)}`)
    console.log(`产物保留在 ${WORK}`)
    process.exit(1)
  }
  if (!keep) fs.rmSync(WORK, { recursive: true, force: true })
  console.log('P4 e2e: 全部通过')
}

await main()
