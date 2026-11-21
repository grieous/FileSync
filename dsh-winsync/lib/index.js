/**
 * dsh-winsync — host half.
 *
 * 职责边界：**不实现任何同步逻辑**。它只做三件事：
 *   1. 管 daemon 的命（发现 / 复用 / 拉起 / 退出）
 *   2. 把 `/winsync/api/*` 代理到 127.0.0.1:<随机端口>，并注入 Bearer
 *   3. 注册四个 agent 工具，让模型能问状态、触发同步
 *
 * 同步算法、调度、状态库全在 `sync-daemon` 里，与 exe 共用同一套 `session::run`。
 * daemon 的状态树是 `%LOCALAPPDATA%\dsh-winsync`，与 exe 的 `%APPDATA%\WinSync`
 * 完全隔离——两边可以同时运行，互不影响。
 */

import os from 'node:os'
import path from 'node:path'

import { DaemonManager } from './daemon.js'
import { loadPrefs } from './prefs.js'
import { createApiHandler } from './proxy.js'
import { registerTools } from './tools.js'

export const name = 'dsh-winsync'
export const inject = ['webServer', 'tools']

const API_PREFIX = '/winsync/api'

/**
 * 插件自己的状态目录。测试或高级用法可以用 `WINSYNC_STATE_DIR` 覆盖，
 * 与 daemon 读的是同一个变量，保证两边始终指向同一棵树。
 */
export function runtimeDir() {
  const override = process.env.WINSYNC_STATE_DIR
  if (override) return override
  const base = process.env.LOCALAPPDATA ?? path.join(os.homedir(), 'AppData', 'Local')
  return path.join(base, 'dsh-winsync')
}

export function apply(ctx) {
  const dir = runtimeDir()
  const prefs = loadPrefs(dir)
  const manager = new DaemonManager({ dir, prefs })

  // 退出时只停自己拉起来的那个；复用别人（比如另一台 dsh 或手动启动）的进程不碰。
  ctx.effect(() => () => manager.stopIfOwned(), 'dsh-winsync: daemon')

  ctx.effect(
    () =>
      ctx.webServer.register({
        kind: 'prefix',
        path: API_PREFIX,
        handler: createApiHandler(manager),
      }),
    'dsh-winsync: api proxy',
  )

  ctx.effect(() => registerTools(ctx, manager), 'dsh-winsync: agent tools')

  // 主动拉起一次，让自动同步在打开面板之前就开始工作。
  // 失败不抛出：面板打开时会重试，并把原因显示在诊断里。
  ctx.effect(() => {
    if (prefs.autoSpawn) {
      manager.ensure().catch((error) => {
        manager.push(`[apply] 预启动失败：${error?.code ?? ''} ${error?.message ?? error}`)
      })
    }
    return () => {}
  }, 'dsh-winsync: eager start')
}
