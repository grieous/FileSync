/**
 * 插件自述：这一版到底是谁。
 *
 * 起因是一个很常见的排查困境——「装了插件，但看不到某个功能」。
 * 面板、tgz、node_modules 里的硬链快照、仓库工作区，四份 `client.js` 可能都不是同一份，
 * 光看界面分不出来。所以让插件自己报出版本号，界面上常驻显示。
 *
 * 只报版本（以及打包时间、安装目录）：commit、分支、是否 dirty 是打包机上的事，
 * 对使用者没有意义，摆出来只是噪音。排查时点一下复制，够定位到「装的是哪一版」。
 *
 * 解析只在模块加载时做一次，不跑子进程。
 */

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

const pkg = readPackage()

export const pluginInfo = Object.freeze({
  name: typeof pkg.name === 'string' ? pkg.name : 'dsh-winsync',
  version: typeof pkg.version === 'string' ? pkg.version : '0.0.0',
  builtAt: typeof pkg.builtAt === 'string' ? pkg.builtAt : null,
  dir: pkgDir,
})

/** 界面上那一小行：`0.8.0`。 */
export const pluginLabel = pluginInfo.version
