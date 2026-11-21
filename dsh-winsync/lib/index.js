/**
 * dsh-winsync — host half (P0 skeleton).
 *
 * P0 scope: prove the three plumbing facts this plugin depends on, nothing else.
 *   1. a `dsh.bundle.patch` row mounts this package as a Cordis plugin
 *   2. `ctx.webServer.register({ kind: 'prefix' })` serves /winsync/api to the page
 *   3. the host half can spawn a child process on a random loopback port, hand it a
 *      bearer token through a 0600-ish daemon.json, proxy its /health, and clean all
 *      of that up when the plugin unloads.
 *
 * The child here is a Node stand-in (`lib/p0-probe.cjs`). P1 replaces it with
 * winsync-daemon.exe; the handshake (port + token + daemon.json) stays identical,
 * so nothing in this file's shape should need to change then.
 */

import { spawn } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import { promises as fsp } from 'node:fs'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

export const name = 'dsh-winsync'
export const inject = ['webServer']

const API_PREFIX = '/winsync/api'
const HERE = path.dirname(fileURLToPath(import.meta.url))
const PROBE_SCRIPT = path.join(HERE, 'p0-probe.cjs')

/**
 * Runtime dir owned by this plugin alone: %LOCALAPPDATA%\dsh-winsync.
 *
 * Hard rule (user requirement): the plugin and WinSync.exe must not affect each
 * other. The exe owns %APPDATA%\WinSync (config.json + its state DB); nothing here
 * reads, writes, locks, or cleans that tree. Our handshake file, PID file, token,
 * and later our own config/DB live in this separate directory, so either side can
 * run, stop, or be uninstalled without touching the other.
 */
function runtimeDir() {
  const base = process.env.LOCALAPPDATA ?? path.join(os.homedir(), 'AppData', 'Local')
  return path.join(base, 'dsh-winsync')
}

function writeJson(res, status, body) {
  const text = JSON.stringify(body)
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store',
    'content-length': Buffer.byteLength(text),
  })
  res.end(text)
}

function loopbackGet(port, routePath, token, timeoutMs = 3000) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      {
        host: '127.0.0.1',
        port,
        path: routePath,
        method: 'GET',
        headers: token === undefined ? {} : { authorization: `Bearer ${token}` },
        timeout: timeoutMs,
      },
      (res) => {
        const chunks = []
        res.on('data', (chunk) => chunks.push(chunk))
        res.on('end', () => {
          resolve({
            status: res.statusCode ?? 0,
            body: Buffer.concat(chunks).toString('utf8'),
          })
        })
      },
    )
    req.on('timeout', () => req.destroy(new Error('loopback timeout')))
    req.on('error', reject)
    req.end()
  })
}

/** True when a pid exists and we may still signal it. */
function pidAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false
  try {
    process.kill(pid, 0)
    return true
  } catch (error) {
    return error?.code === 'EPERM'
  }
}

/** One spawned child: its port, its token, and its recent output for diagnosis. */
class Probe {
  constructor(dir) {
    this.dir = dir
    this.file = path.join(dir, 'daemon.json')
    this.child = null
    this.info = null
    this.starting = null
    this.tail = []
  }

  push(line) {
    this.tail.push(line)
    if (this.tail.length > 40) this.tail.splice(0, this.tail.length - 40)
  }

  alive() {
    return this.child !== null && this.child.exitCode === null && this.child.signalCode === null
  }

  /** Spawn once; concurrent callers share the same in-flight promise. */
  async start() {
    if (this.alive() && this.info !== null) return this.info
    if (this.starting !== null) return this.starting
    this.starting = this.#spawn().finally(() => {
      this.starting = null
    })
    return this.starting
  }

  async #spawn() {
    await fsp.mkdir(this.dir, { recursive: true })
    await this.#reclaimStaleHandshake()

    const token = randomBytes(24).toString('hex')
    const child = spawn(process.execPath, [PROBE_SCRIPT], {
      env: {
        ...process.env,
        // Electron's execPath re-enters as plain Node only with this flag; it is how
        // the shipped `dsh` CLI launches itself. Harmless for a native daemon later.
        ELECTRON_RUN_AS_NODE: '1',
        WINSYNC_PROBE_DIR: this.dir,
        WINSYNC_TOKEN: token,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    })
    this.child = child
    child.stdout?.setEncoding('utf8').on('data', (chunk) => this.push(String(chunk).trimEnd()))
    child.stderr?.setEncoding('utf8').on('data', (chunk) => this.push(`[stderr] ${String(chunk).trimEnd()}`))
    child.on('exit', (code, signal) => {
      this.push(`[probe] exited code=${code} signal=${signal}`)
      this.info = null
    })

    const info = await this.#waitForInfo(8000)
    info.token = token
    this.info = info
    return info
  }

  /**
   * A daemon.json left by a host that was killed outright (taskkill, crash, power
   * loss) outlives its child: the host's cleanup effect never ran. Reclaim it
   * before spawning so a stale port/token is never handed to the page, and so a
   * surviving orphan (pid still alive) is reported rather than silently leaked.
   */
  async #reclaimStaleHandshake() {
    let stale
    try {
      stale = JSON.parse(await fsp.readFile(this.file, 'utf8'))
    } catch {
      await fsp.rm(this.file, { force: true })
      return
    }
    if (pidAlive(stale?.pid)) {
      this.push(`[reclaim] orphan pid=${stale.pid} from a previous host is still alive; replacing handshake`)
      try {
        process.kill(stale.pid)
      } catch {
        /* it may exit on its own; the new handshake is authoritative either way */
      }
    } else {
      this.push(`[reclaim] dropped stale daemon.json (pid=${stale?.pid ?? 'unknown'} is gone)`)
    }
    await fsp.rm(this.file, { force: true })
  }

  async #waitForInfo(timeoutMs) {
    const deadline = Date.now() + timeoutMs
    let lastError = 'no daemon.json yet'
    while (Date.now() < deadline) {
      try {
        const parsed = JSON.parse(await fsp.readFile(this.file, 'utf8'))
        if (typeof parsed?.port === 'number' && parsed.port > 0) return parsed
        lastError = 'daemon.json has no port'
      } catch (error) {
        lastError = error instanceof Error ? error.message : String(error)
      }
      if (!this.alive()) {
        throw new Error(`probe exited before writing daemon.json (${lastError})`)
      }
      await new Promise((resolve) => setTimeout(resolve, 100))
    }
    throw new Error(`timed out waiting for daemon.json (${lastError})`)
  }

  async stop() {
    const child = this.child
    this.child = null
    this.info = null
    if (child !== null && child.exitCode === null) {
      child.kill()
      await new Promise((resolve) => {
        const timer = setTimeout(resolve, 1500)
        child.once('exit', () => {
          clearTimeout(timer)
          resolve()
        })
      })
    }
    await fsp.rm(this.file, { force: true })
  }
}

export function apply(ctx) {
  const dir = runtimeDir()
  const probe = new Probe(dir)

  // Cleanup belongs to this plugin's fiber: unloading the plugin stops the child and
  // removes the handshake file, so no stale token or orphan process survives a reload.
  ctx.effect(() => () => {
    void probe.stop()
  })

  ctx.effect(() =>
    ctx.webServer.register({
      kind: 'prefix',
      path: API_PREFIX,
      handler: async (req, res) => {
        const url = new URL(req.url ?? '/', 'http://127.0.0.1')
        const route = url.pathname.slice(API_PREFIX.length) || '/'

        try {
          if (route === '/ping') {
            writeJson(res, 200, { ok: true, plugin: name, api: API_PREFIX, runtimeDir: dir })
            return
          }

          if (route === '/probe/start' && req.method === 'POST') {
            const info = await probe.start()
            const health = await loopbackGet(info.port, '/health', info.token)
            writeJson(res, 200, {
              ok: true,
              pid: info.pid,
              port: info.port,
              daemonFile: probe.file,
              health: { status: health.status, body: safeJson(health.body) },
            })
            return
          }

          if (route === '/probe/status') {
            if (!probe.alive() || probe.info === null) {
              writeJson(res, 200, { ok: true, running: false, log: probe.tail })
              return
            }
            const health = await loopbackGet(probe.info.port, '/health', probe.info.token)
            writeJson(res, 200, {
              ok: true,
              running: true,
              pid: probe.info.pid,
              port: probe.info.port,
              health: { status: health.status, body: safeJson(health.body) },
              log: probe.tail,
            })
            return
          }

          if (route === '/probe/stop' && req.method === 'POST') {
            await probe.stop()
            writeJson(res, 200, { ok: true, running: false })
            return
          }

          writeJson(res, 404, { ok: false, error: { code: 'not-found', message: `no route ${route}` } })
        } catch (error) {
          writeJson(res, 500, {
            ok: false,
            error: { code: 'probe-failed', message: error instanceof Error ? error.message : String(error) },
            log: probe.tail,
          })
        }
      },
    }),
  )
}

function safeJson(text) {
  try {
    return JSON.parse(text)
  } catch {
    return text
  }
}
