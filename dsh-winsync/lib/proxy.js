/**
 * 浏览器 ↔ daemon 的唯一通道。
 *
 * 页面只认 `/winsync/api/*`（挂在 dsh 自己的 server 上，天然同源、天然有会话），
 * 这里再补三件事：
 *   1. 注入 Bearer 与 `X-WinSync-Client`——token 不进浏览器
 *   2. 限制方法与体积，别让它变成任意的本机写入口
 *   3. SSE 逐块直通，不缓冲、不聚合
 *
 * 上游 5xx 一律翻成 `502 daemon-unavailable`：界面只需要知道「同步服务没起来」。
 */

import http from 'node:http'

const ALLOWED_METHODS = new Set(['GET', 'POST', 'PATCH', 'DELETE'])
const MAX_BODY_BYTES = 256 * 1024

function writeJson(res, status, body) {
  if (res.headersSent) {
    res.end()
    return
  }
  const text = JSON.stringify(body)
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store',
    'content-length': Buffer.byteLength(text),
  })
  res.end(text)
}

function failure(code, message, detail) {
  return { ok: false, error: { code, message, ...(detail === undefined ? {} : { detail }) } }
}

function forwardHeaders(req) {
  const headers = { accept: 'application/json', 'x-winsync-client': 'dsh-plugin' }
  const contentType = req.headers['content-type']
  if (typeof contentType === 'string') headers['content-type'] = contentType
  return headers
}

/**
 * 握手文件里最要命的是 token：它是 daemon 的唯一凭证。
 * /diag 只证明「握手存在、pid 对不对」，所以只给指纹与长度。
 */
function redactHandshake(handshake) {
  if (!handshake) return null
  const token = typeof handshake.token === 'string' ? handshake.token : ''
  return {
    pid: handshake.pid ?? null,
    port: handshake.port ?? null,
    version: handshake.version ?? null,
    started_at: handshake.started_at ?? null,
    token: {
      present: token.length > 0,
      length: token.length,
      fingerprint: token.length >= 8 ? `${token.slice(0, 4)}…${token.slice(-4)}` : '…',
    },
  }
}

/**
 * 建一个 prefix handler。
 * @param {import('./daemon.js').DaemonManager} manager
 */
export function createApiHandler(manager) {
  return async function handle(req, res) {
    const url = new URL(req.url ?? '/', 'http://127.0.0.1')
    const route = url.pathname.replace(/^\/winsync\/api/, '') || '/'

    // 诊断用：不碰 daemon 也能回答「插件装没装上、状态目录在哪」
    if (route === '/ping') {
      writeJson(res, 200, {
        ok: true,
        plugin: 'dsh-winsync',
        api: '/winsync/api',
        runtimeDir: manager.dir,
        daemon: manager.snapshot(),
      })
      return
    }

    if (route === '/diag') {
      let health = null
      try {
        const info = await manager.ensure()
        const reply = await manager.request('/health')
        health = { port: info.port, pid: info.pid, status: reply.status, body: reply.body }
      } catch (error) {
        health = { error: { code: error?.code ?? 'daemon-unavailable', message: error?.message ?? String(error) } }
      }
      writeJson(res, 200, {
        ok: true,
        plugin: 'dsh-winsync',
        runtimeDir: manager.dir,
        prefsPath: manager.snapshot().prefsPath,
        handshake: redactHandshake(manager.peekHandshake()),
        daemon: manager.snapshot(),
        health,
      })
      return
    }


    if (!ALLOWED_METHODS.has(req.method ?? 'GET')) {
      writeJson(res, 405, failure('bad-request', `不支持的方法 ${req.method}`))
      return
    }

    let info
    try {
      info = await manager.ensure()
    } catch (error) {
      writeJson(res, 502, failure(error?.code ?? 'daemon-unavailable', error?.message ?? String(error), error?.detail))
      return
    }

    const isSse = route === '/events'
    await new Promise((resolve) => {
      let settled = false
      const finish = () => {
        if (!settled) {
          settled = true
          resolve()
        }
      }

      const upstream = http.request(
        {
          host: '127.0.0.1',
          port: info.port,
          // daemon 的契约前缀是 /api，这里补上；route 已经是去掉插件前缀后的路径
          path: `/api${route}${url.search}`,
          method: req.method,
          headers: { ...forwardHeaders(req), authorization: `Bearer ${info.token}` },
          timeout: isSse ? 0 : 60_000,
        },
        (up) => {
          if ((up.statusCode ?? 502) >= 500) {
            up.resume()
            writeJson(res, 502, failure('daemon-unavailable', `daemon 返回 HTTP ${up.statusCode}`))
            finish()
            return
          }
          const headers = { 'cache-control': 'no-store' }
          const contentType = up.headers['content-type']
          if (typeof contentType === 'string') headers['content-type'] = contentType
          if (!isSse && up.headers['content-length']) {
            headers['content-length'] = up.headers['content-length']
          }
          res.writeHead(up.statusCode ?? 502, headers)
          up.pipe(res)
          up.on('end', finish)
          up.on('error', () => {
            res.end()
            finish()
          })
          // 浏览器断开就掐掉上游，别让 daemon 对着空气写
          res.on('close', () => up.destroy())
        },
      )

      upstream.on('timeout', () => upstream.destroy(new Error('upstream timeout')))
      upstream.on('error', (error) => {
        writeJson(res, 502, failure('daemon-unavailable', `连不上同步服务：${error.message}`))
        finish()
      })

      let size = 0
      req.on('data', (chunk) => {
        size += chunk.length
        if (size > MAX_BODY_BYTES) {
          upstream.destroy()
          writeJson(res, 413, failure('bad-request', '请求体超过 256 KB'))
          finish()
        }
      })
      req.on('error', () => {
        upstream.destroy()
        finish()
      })
      req.pipe(upstream)
    })
  }
}
