/**
 * 插件自述：这一版到底是谁。
 *
 * 起因是一个很常见的排查困境——「装了插件，但看不到某个功能」。
 * 面板、tgz、node_modules 里的硬链快照、仓库工作区，四份 `client.js` 可能都不是同一份，
 * 光看界面分不出来。所以让插件自己报出版本与 commit，界面上常驻显示。
 *
 * 两个来源：
 *   - 打包过的（tgz 装进 node_modules）：版本与 commit 由 `scripts/pack.mjs` 写进 package.json，
 *     这里直接读，不依赖机器上有没有 git。
 *   - 开发态（`dsh plugin add file:<仓库目录>`）：package.json 没有注入字段，退回问 git；
 *     工作区有未提交改动时打 `dirty`，提醒「界面上的行为不等于任何一次提交」。
 *
 * 解析只在模块加载时做一次：`git` 是子进程，不能放到每次请求里。
 */

import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const pkgDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')

function readPackage() {
  try {
    return JSON.parse(readFileSync(path.join(pkgDir, 'package.json'), 'utf8'))
  } catch {
    return {}
  }
}

/** git 不可用、或当前目录不在仓库里时返回 null——这是正常情况，不是错误。 */
function git(args) {
  try {
    return execFileSync('git', ['-C', pkgDir, ...args], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
      timeout: 1500,
    }).trim()
  } catch {
    return null
  }
}

const pkg = readPackage()

// package.json 里的注入字段优先：打包后的产物要能复现「我当时从哪个提交打的」
const injectedCommit = typeof pkg.gitCommit === 'string' && pkg.gitCommit ? pkg.gitCommit : null
const gitCommit = git(['rev-parse', '--short', 'HEAD'])
const commit = injectedCommit ?? (gitCommit || null)

const injectedDirty = typeof pkg.gitDirty === 'boolean' ? pkg.gitDirty : null
// git status --porcelain：空字符串=干净，非空=有改动，null=问不到 git
const porcelain = injectedCommit ? null : git(['status', '--porcelain'])
const dirty = injectedDirty ?? (porcelain === null ? false : porcelain !== '')

const branch =
  (typeof pkg.gitBranch === 'string' && pkg.gitBranch ? pkg.gitBranch : null) ??
  (injectedCommit ? null : git(['rev-parse', '--abbrev-ref', 'HEAD']) || null)

export const pluginInfo = Object.freeze({
  name: typeof pkg.name === 'string' ? pkg.name : 'dsh-winsync',
  version: typeof pkg.version === 'string' ? pkg.version : '0.0.0',
  commit,
  // 打包注入 / 现场问 git / 都没有，排查时能立刻知道这个字段可不可信
  commitSource: injectedCommit ? 'package.json' : commit ? 'git' : null,
  branch,
  dirty,
  builtAt: typeof pkg.builtAt === 'string' ? pkg.builtAt : null,
  dir: pkgDir,
})

/** 界面上那一小行：`0.5.3 · c76a83c`，工作区有改动时加 `+dirty`。 */
export const pluginLabel = [
  pluginInfo.version,
  pluginInfo.commit ? `${pluginInfo.commit}${pluginInfo.dirty ? '+dirty' : ''}` : null,
]
  .filter(Boolean)
  .join(' · ')
