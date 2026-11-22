/**
 * daemon 生命周期：发现 → 复用 → 拉起 → 退出。
 *
 * 握手完全走 `daemon.json`（pid/port/token），与 daemon 侧的实现一一对应：
 *   - 文件在、/health 通  → 复用（owned=false，退出时不杀）
 *   - 文件在、进程已死    → 删掉陈旧握手，重新拉起
 *   - 文件在、进程还在但没响应 → 视为「正在启动/卡住」，报 busy，不重复拉
 *   - 没文件              → 找二进制并 spawn，等到 /health 通
 *
 * token 只在本模块内存里流转，用于给上游请求注入 Authorization；
 * 它不会出现在任何返回给浏览器的响应里，也不会进模型上下文。
 */

import { spawn } from 'node:child_process'
import { existsSync, promises as fsp, readFileSync } from 'node:fs'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

import { prefsPath } from './prefs.js'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const HANDSHAKE = 'daemon.json'
const HEALTH_TIMEOUT_MS = 400
const START_TIMEOUT_MS = 8000

/** 带 code 的错误：代理层直接翻成 `{ok:false,error:{code,message}}`。 */
export class DaemonError extends Error {
  constructor(code, message, detail) {
    super(message)
    this.name = 'DaemonError'
    this.code = code
    this.detail = detail
  }
}

/** pid 是否还能被 signal（进程存在即可，不要求我们有权限）。 */
export function pidAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false
  try {
    process.kill(pid, 0)
    return true
  } catch (error) {
    return error?.code === 'EPERM'
  }
}

/** 一次 loopback JSON 请求，返回 {status, body}；不抛网络异常（网络问题也走返回值）。 */
export function loopbackJson(port, route, { method = 'GET', token, body, timeoutMs = 5000 } = {}) {
  return new Promise((resolve, reject) => {
    const payload = body === undefined ? null : Buffer.from(JSON.stringify(body), 'utf8')
    const headers = { accept: 'application/json' }
    if (token !== undefined) headers.authorization = `Bearer ${token}`
    if (payload !== null) {
      headers['content-type'] = 'application/json'
      headers['content-length'] = String(payload.length)
    }
    const request = http.request(
      { host: '127.0.0.1', port, path: route, method, headers, timeout: timeoutMs },
      (response) => {
        const chunks = []
        response.on('data', (chunk) => chunks.push(chunk))
        response.on('end', () => {
          const text = Buffer.concat(chunks).toString('utf8')
          let parsed
          try {
            parsed = text === '' ? null : JSON.parse(text)
          } catch {
            parsed = { raw: text }
          }
          resolve({ status: response.statusCode ?? 0, body: parsed })
        })
      },
    )
    request.on('timeout', () => request.destroy(new Error('loopback timeout')))
    request.on('error', reject)
    if (payload !== null) request.write(payload)
    request.end()
  })
}

export class DaemonManager {
  /**
   * @param {object} options
   * @param {string} options.dir       状态目录（%LOCALAPPDATA%\dsh-winsync）
   * @param {object} options.prefs     见 prefs.js
   */
  constructor({ dir, prefs }) {
    this.dir = dir
    this.prefs = prefs
    this.file = path.join(dir, HANDSHAKE)
    this.child = null
    this.info = null
    this.owned = false
    this.starting = null
    this.tail = []
  }

  push(line) {
    this.tail.push(line)
    if (this.tail.length > 60) this.tail.splice(0, this.tail.length - 60)
  }

  /** 给界面/工具看的轻量状态，不含 token。 */
  snapshot() {
    return {
      running: this.info !== null,
      owned: this.owned,
      pid: this.info?.pid ?? null,
      port: this.info?.port ?? null,
      version: this.info?.version ?? null,
      stateDir: this.dir,
      prefsPath: prefsPath(this.dir),
      binary: this.info === null ? null : this.resolvedBinary,
      log: this.tail.slice(-40),
    }
  }

  /** 二进制查找顺序：偏好 → 环境变量 → 安装目录 → PATH → 仓库开发产物。 */
  resolveBinary() {
    const candidates = []
    if (this.prefs.binPath) candidates.push(this.prefs.binPath)
    if (process.env.WINSYNC_DAEMON_BIN) candidates.push(process.env.WINSYNC_DAEMON_BIN)
    const local = process.env.LOCALAPPDATA ?? path.join(os.homedir(), 'AppData', 'Local')
    // 随插件一起装的（npm 包里带 bin/），只装 dsh 插件也能跑
    candidates.push(path.resolve(HERE, '..', 'bin', 'winsync-daemon.exe'))
    // 插件自己的数据目录：用户手动放进去的副本
    candidates.push(path.join(this.dir, 'bin', 'winsync-daemon.exe'))
    candidates.push(path.join(local, 'Programs', 'WinSync', 'winsync-daemon.exe'))
    for (const dir of (process.env.PATH ?? '').split(path.delimiter)) {
      if (dir) candidates.push(path.join(dir, 'winsync-daemon.exe'))
    }
    // 仓库内开发：dsh-winsync/lib → <repo>/sync/target/debug/sync-daemon.exe
    candidates.push(path.resolve(HERE, '..', '..', 'sync', 'target', 'debug', 'sync-daemon.exe'))
    candidates.push(path.resolve(HERE, '..', '..', 'sync', 'target', 'release', 'sync-daemon.exe'))

    for (const candidate of candidates) {
      try {
        if (candidate && existsSync(candidate)) return candidate
      } catch {
        /* 路径非法就跳过 */
      }
    }
    throw new DaemonError(
      'daemon-missing',
      '找不到 winsync-daemon.exe。请先 cargo build -p sync-daemon，或在插件偏好里设置 binPath',
      { tried: candidates.filter(Boolean) },
    )
  }

  async readHandshake() {
    try {
      const parsed = JSON.parse(await fsp.readFile(this.file, 'utf8'))
      if (typeof parsed?.port === 'number' && parsed.port > 0 && typeof parsed.token === 'string') {
        return parsed
      }
      return null
    } catch {
      return null
    }
  }

  async healthOk(info) {
    try {
      const reply = await loopbackJson(info.port, '/health', { timeoutMs: HEALTH_TIMEOUT_MS })
      return reply.status === 200 && reply.body?.ok === true
    } catch {
      return false
    }
  }

  /** 已经连上就直接用；否则按握手文件判断复用还是拉起。 */
  async ensure() {
    if (this.info !== null) {
      if (await this.healthOk(this.info)) return this.info
      this.push('[ensure] 现有连接不再响应，重新发现')
      this.info = null
    }
    if (this.starting !== null) return this.starting
    this.starting = this.#discoverOrSpawn().finally(() => {
      this.starting = null
    })
    return this.starting
  }

  async #discoverOrSpawn() {
    await fsp.mkdir(this.dir, { recursive: true })

    const existing = await this.readHandshake()
    if (existing !== null) {
      if (await this.healthOk(existing)) {
        this.info = existing
        this.owned = false
        this.push(`[ensure] 复用已在运行的 daemon pid=${existing.pid} port=${existing.port}`)
        return existing
      }
      if (pidAlive(existing.pid)) {
        // 进程在、health 不通：可能正在启动，也可能卡住。不抢，交给上层重试。
        throw new DaemonError(
          'daemon-busy',
          `daemon 进程 ${existing.pid} 还在但不响应 /health，请稍后重试或结束该进程`,
        )
      }
      this.push(`[ensure] 丢弃陈旧握手（pid=${existing.pid} 已退出）`)
      await fsp.rm(this.file, { force: true })
    }

    if (this.prefs.autoSpawn === false) {
      throw new DaemonError(
        'daemon-unavailable',
        '同步服务未启动，且偏好里关闭了自动拉起（autoSpawn=false）',
      )
    }
    return this.#spawn()
  }

  async #spawn() {
    const binary = this.resolveBinary()
    this.resolvedBinary = binary
    const child = spawn(binary, [], {
      env: { ...process.env, WINSYNC_STATE_DIR: this.dir },
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    })
    this.child = child
    this.owned = true
    child.stdout?.setEncoding('utf8').on('data', (chunk) => this.push(String(chunk).trimEnd()))
    child.stderr?.setEncoding('utf8').on('data', (chunk) => this.push(`[stderr] ${String(chunk).trimEnd()}`))
    child.on('exit', (code, signal) => {
      this.push(`[daemon] exited code=${code} signal=${signal}`)
      if (this.child === child) {
        this.child = null
        this.info = null
        this.owned = false
      }
    })

    const deadline = Date.now() + START_TIMEOUT_MS
    let lastError = 'daemon.json 还没出现'
    while (Date.now() < deadline) {
      const info = await this.readHandshake()
      if (info !== null && (await this.healthOk(info))) {
        this.info = info
        this.push(`[ensure] 已拉起 pid=${info.pid} port=${info.port}`)
        return info
      }
      if (child.exitCode !== null) {
        throw new DaemonError(
          'daemon-exited',
          `daemon 启动后立即退出（code=${child.exitCode}）：${this.tail.slice(-3).join(' | ')}`,
          { binary },
        )
      }
      await new Promise((resolve) => setTimeout(resolve, 120))
    }
    throw new DaemonError('daemon-timeout', `等待 daemon 就绪超时（${lastError}）`, { binary })
  }

  /** 调 daemon 的 JSON 接口（工具用；代理走自己的流式通道）。 */
  async request(route, options = {}) {
    const info = await this.ensure()
    const reply = await loopbackJson(info.port, route, { ...options, token: info.token })
    return { info, ...reply }
  }

  /** 解析 `{ok:false,error}`，翻成带 code 的异常，给工具/代理统一处理。 */
  static unwrap(reply) {
    if (reply.body?.ok === false) {
      const { code, message } = reply.body.error ?? {}
      throw new DaemonError(code ?? 'daemon-error', message ?? `HTTP ${reply.status}`)
    }
    if (reply.status >= 400) {
      throw new DaemonError('daemon-unavailable', `daemon 返回 HTTP ${reply.status}`)
    }
    return reply.body
  }

  /** 只停我们自己拉起来的那个；复用别人的进程一律不碰。 */
  async stopIfOwned() {
    if (!this.owned) return
    if (this.prefs.stopDaemonOnExit === false) {
      this.push('[stop] 偏好要求常驻，保持 daemon 运行')
      return
    }
    const child = this.child
    const info = this.info
    if (info !== null) {
      try {
        await loopbackJson(info.port, '/api/shutdown', {
          method: 'POST',
          token: info.token,
          timeoutMs: 1500,
        })
      } catch {
        /* 优雅退出失败就退化为 kill */
      }
    }
    if (child !== null && child.exitCode === null) {
      const exited = new Promise((resolve) => {
        const timer = setTimeout(resolve, 1500)
        child.once('exit', () => {
          clearTimeout(timer)
          resolve()
        })
      })
      await exited
      if (child.exitCode === null) {
        try {
          child.kill()
        } catch {
          /* 已经没了 */
        }
      }
    }
    await fsp.rm(this.file, { force: true })
    this.child = null
    this.info = null
    this.owned = false
  }

  /** 诊断用：同步读一次握手文件（不校验存活）。 */
  peekHandshake() {
    try {
      return JSON.parse(readFileSync(this.file, 'utf8'))
    } catch {
      return null
    }
  }
}
