/**
 * 种子展开的单元测试（纯 Node，不需要 dsh、不需要 Windows）。
 *
 * 走的是真代码：造一个假的状态目录，让 DaemonManager 自己去读包内
 * `bin/winsync-daemon.exe.gz`、展开、再读一遍。断言四件事：
 *
 *   1. 首次展开的字节与源 exe 完全一致（sha256 相等）
 *   2. 第二次调用不再重写（mtime 不变），记录里 reused=true
 *   3. 种子换了（gz 内容变）会重新展开
 *   4. 种子坏了不会炸掉整个插件：已展开的旧副本还能用
 *
 *   node test/seed.mjs
 */

import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { gzipSync, gunzipSync } from 'node:zlib'

import { DaemonManager } from '../lib/daemon.js'

const pkgDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const seedPath = path.join(pkgDir, 'bin', 'winsync-daemon.exe.gz')

if (!existsSync(seedPath)) {
  console.error(
    [
      `缺少 ${path.relative(pkgDir, seedPath)}`,
      '',
      '先放好 exe 再打包/测试：',
      '  mkdir -p bin && 把 winsync-daemon.exe 放进 dsh-winsync/bin/',
      '  node scripts/pack.mjs   （它会顺带产出 bin/winsync-daemon.exe.gz）',
    ].join('\n'),
  )
  process.exit(1)
}

const source = gunzipSync(readFileSync(seedPath))
const sourceDigest = createHash('sha256').update(source).digest('hex')

const root = mkdtempSync(path.join(os.tmpdir(), 'winsync-seed-'))
const stateDir = path.join(root, 'state')
const target = path.join(stateDir, 'bin', 'winsync-daemon.exe')
const marker = path.join(stateDir, 'bin', 'seed.json')

const make = () =>
  new DaemonManager({ dir: stateDir, prefs: { binPath: '', autoSpawn: true, stopDaemonOnExit: true } })

const digestOf = (file) => createHash('sha256').update(readFileSync(file)).digest('hex')

try {
  // ---- 1. 首次展开
  const manager = make()
  const seeded = manager.seedBinary()
  assert.equal(seeded, target, '种子没有展开到状态目录')
  assert.ok(existsSync(target), '展开后的文件不存在')
  assert.equal(digestOf(target), sourceDigest, '展开后的字节与源 exe 不一致')
  assert.equal(statSync(target).size, source.length, '展开后的体积不对')
  assert.equal(manager.seed.reused, false, '首次应当是新展开')
  const record = JSON.parse(readFileSync(marker, 'utf8'))
  assert.match(record.sha256, /^[0-9a-f]{64}$/, '记录里没有 sha256')

  // ---- 2. 幂等：不再重写
  const firstMtime = statSync(target).mtimeMs
  const again = make()
  assert.equal(again.seedBinary(), target, '第二次没有复用已有副本')
  assert.equal(again.seed.reused, true, '第二次应当是复用')
  assert.equal(statSync(target).mtimeMs, firstMtime, '第二次不该重写文件')

  // ---- 3. 种子换了要重展开
  const backup = readFileSync(seedPath)
  const tweaked = Buffer.concat([gunzipSync(backup), Buffer.from('VERSION-BUMP')])
  const before = statSync(target).mtimeMs
  writeFileSync(seedPath, gzipSync(tweaked))
  const bumped = make()
  assert.equal(bumped.seedBinary(), target, '换了种子后路径不对')
  assert.equal(bumped.seed.reused, false, '换了种子应当重展开')
  assert.equal(statSync(target).size, tweaked.length, '重展开后体积没有跟着变')
  assert.ok(statSync(target).mtimeMs >= before, 'mtime 没有前进')

  // ---- 4. 种子坏了：不抛，继续用旧副本
  writeFileSync(seedPath, Buffer.from('这不是 gzip'))
  const broken = make()
  assert.equal(broken.seedBinary(), target, '种子坏了应当退回已有副本')
  assert.equal(broken.seed.error, 'gunzip-failed', '没有标记失败原因')
  assert.equal(digestOf(target), createHash('sha256').update(tweaked).digest('hex'), '旧副本被破坏了')

  // 还原种子，别把仓库搞脏
  writeFileSync(seedPath, backup)

  // ---- 5. 状态目录被清空后能重建（用户手删 state 目录 / 换机器）
  rmSync(stateDir, { recursive: true, force: true })
  const fresh = make()
  assert.equal(fresh.seedBinary(), target, '状态目录清空后没有重新展开')
  assert.equal(digestOf(target), sourceDigest, '重建出来的不是同一个 exe')
  console.log('seed: 种子展开与幂等性通过')
} finally {
  rmSync(root, { recursive: true, force: true })
}
