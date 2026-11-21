/**
 * 插件偏好：`<state>\plugin.json`。
 *
 * 放在我们自己的状态树里，不碰 exe 的 `%APPDATA%\WinSync`——用户硬要求：
 * 两边互不影响，所以连一个偏好文件都不共享。
 *
 * 字段：
 *   binPath           显式指定 daemon 可执行文件；空则按内置顺序查找
 *   stopDaemonOnExit  插件卸载时是否停掉「由本插件拉起」的 daemon（默认 true）
 *   autoSpawn         界面第一次请求时是否自动拉起（false 时只连已有的）
 */

import { readFileSync, writeFileSync } from 'node:fs'
import path from 'node:path'

export const DEFAULT_PREFS = {
  binPath: '',
  stopDaemonOnExit: true,
  autoSpawn: true,
}

export function prefsPath(dir) {
  return path.join(dir, 'plugin.json')
}

/** 读偏好。文件坏了不能拖垮插件：回默认值并在返回值里说明。 */
export function loadPrefs(dir) {
  try {
    const raw = readFileSync(prefsPath(dir), 'utf8')
    const parsed = JSON.parse(raw)
    const merged = { ...DEFAULT_PREFS }
    for (const key of Object.keys(DEFAULT_PREFS)) {
      if (parsed?.[key] !== undefined) merged[key] = parsed[key]
    }
    merged.binPath = typeof merged.binPath === 'string' ? merged.binPath.trim() : ''
    merged.stopDaemonOnExit = merged.stopDaemonOnExit !== false
    merged.autoSpawn = merged.autoSpawn !== false
    return merged
  } catch {
    return { ...DEFAULT_PREFS }
  }
}

export function savePrefs(dir, prefs) {
  writeFileSync(prefsPath(dir), JSON.stringify({ ...DEFAULT_PREFS, ...prefs }, null, 2), 'utf8')
}
