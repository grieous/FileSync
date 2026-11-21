/**
 * P0 probe — a stand-in for winsync-daemon.exe.
 *
 * It does exactly what the real daemon's handshake must do, and nothing more:
 * bind an ephemeral loopback port, write {pid, port, startedAt} to daemon.json in
 * the shared runtime dir, answer /health behind a bearer token, and exit cleanly
 * when the parent removes it.
 *
 * CJS on purpose: the package is `"type": "module"`, and this file must run under
 * plain `node <file>` with no loader flags.
 */

const http = require('node:http')
const fs = require('node:fs')
const path = require('node:path')

const dir = process.env.WINSYNC_PROBE_DIR
const token = process.env.WINSYNC_TOKEN

if (!dir || !token) {
  console.error('[probe] WINSYNC_PROBE_DIR and WINSYNC_TOKEN are required')
  process.exit(2)
}

const startedAt = new Date().toISOString()

const server = http.createServer((req, res) => {
  const authorized = req.headers.authorization === `Bearer ${token}`
  const send = (status, body) => {
    const text = JSON.stringify(body)
    res.writeHead(status, {
      'content-type': 'application/json; charset=utf-8',
      'cache-control': 'no-store',
      'content-length': Buffer.byteLength(text),
    })
    res.end(text)
  }

  if (!authorized) {
    send(401, { ok: false, error: 'unauthorized' })
    return
  }
  if (req.url === '/health') {
    send(200, {
      ok: true,
      kind: 'p0-probe',
      pid: process.pid,
      startedAt,
      uptimeMs: Math.round(process.uptime() * 1000),
      node: process.version,
    })
    return
  }
  send(404, { ok: false, error: 'not-found' })
})

server.listen(0, '127.0.0.1', () => {
  const address = server.address()
  const port = typeof address === 'object' && address !== null ? address.port : 0
  const file = path.join(dir, 'daemon.json')
  const body = JSON.stringify({ pid: process.pid, port, startedAt }, null, 2)
  fs.writeFileSync(file, body, { mode: 0o600 })
  console.log(`[probe] pid=${process.pid} listening=127.0.0.1:${port}`)
  console.log(`[probe] handshake=${file}`)
})

let closing = false
function shutdown(reason) {
  if (closing) return
  closing = true
  console.log(`[probe] shutting down (${reason})`)
  const file = path.join(dir, 'daemon.json')
  try {
    const current = JSON.parse(fs.readFileSync(file, 'utf8'))
    if (current.pid === process.pid) fs.unlinkSync(file)
  } catch {
    /* already gone */
  }
  server.close(() => process.exit(0))
  setTimeout(() => process.exit(0), 1000).unref()
}

process.on('SIGTERM', () => shutdown('SIGTERM'))
process.on('SIGINT', () => shutdown('SIGINT'))
process.on('disconnect', () => shutdown('parent disconnect'))
// The parent owns our lifetime; if it dies without a signal, we still exit.
if (typeof process.send === 'function') process.on('message', (message) => {
  if (message === 'shutdown') shutdown('message')
})
