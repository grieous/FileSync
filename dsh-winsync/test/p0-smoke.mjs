/**
 * P0 smoke test — no dsh, no browser, no Rust.
 *
 * It reproduces the two contracts this package must satisfy, so a regression is
 * caught here instead of by restarting the desktop app:
 *
 *   1. browser half: the bundle registers its factory through
 *      `window.__ModuleLoader__.load({ id, factory })`, the factory returns a
 *      Cordis client plugin, and `apply(ctx)` registers the `main` panel plus
 *      the `sidebar.panellist` row that opens it, both under one id.
 *   2. host half: `apply(ctx)` registers a `prefix` route at /winsync/api and the
 *      handler answers /ping over a real socket.
 *
 * Run: node test/p0-smoke.mjs
 */

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import http from 'node:http'

/* ---------------------------------------------------------------- browser half */

const bundle = readFileSync(new URL('../lib/client.js', import.meta.url), 'utf8')

let captured
const fakeReact = {
  createElement: (type, props, ...children) => ({ type, props, children }),
  Fragment: Symbol('Fragment'),
  useState: (initial) => [typeof initial === 'function' ? initial() : initial, () => {}],
  useEffect: () => {},
  useCallback: (fn) => fn,
}
const fakeWindow = {
  __ModuleLoader__: {
    load({ id, factory }) {
      captured = { id, factory }
    },
  },
}
new Function('window', bundle)(fakeWindow)

assert.ok(captured, 'bundle did not call window.__ModuleLoader__.load')
assert.equal(captured.id, 'dsh-winsync', 'module id must equal the package name')

const clientPlugin = captured.factory((specifier) => {
  if (specifier === 'react') return fakeReact
  throw new Error(`bundle requested a non-baseline module: ${specifier}`)
})
assert.equal(clientPlugin.name, 'dsh-winsync')
assert.deepEqual(clientPlugin.inject, ['slots'])

const registrations = []
clientPlugin.apply({
  slots: {
    inject(key, callback) {
      registrations.push({ kind: 'inject', key })
      return callback()
    },
    register(options, component) {
      registrations.push({ kind: 'register', options, component })
      return () => {}
    },
  },
})

const injectedSlots = registrations.filter((entry) => entry.kind === 'inject').map((entry) => entry.key)
assert.deepEqual(injectedSlots.sort(), ['main', 'sidebar.panellist'], 'left sidebar entry + its main panel are the only surfaces')

const entries = registrations.filter((r) => r.kind === 'register')
const page = entries.find((r) => r.options.name === 'main')
assert.equal(page?.options.key, 'winsync', 'main panel key must match the sidebar row id')
assert.equal(typeof page.component, 'function')

const row = entries.find((r) => r.options.name === 'sidebar.panellist')
assert.equal(row?.options.id, 'winsync', 'sidebar row id addresses the main panel')
assert.equal(row?.options.order, 10)
assert.equal(typeof row?.options.label, 'function')
assert.equal(row.options.label(), '同步')
assert.equal(typeof row.component, 'function')

// Both components must survive a bare call: the panel shell renders, and the
// sidebar glyph draws at the size the sidebar passes (hooks are no-ops here).
const tree = page.component({})
assert.ok(tree && typeof tree === 'object', 'WinsyncPage returned nothing')
const glyph = row.component({ size: 20, active: false })
assert.equal(glyph.type, 'svg')
assert.equal(glyph.props.width, 20)
assert.equal(glyph.props.height, 20)

/* ------------------------------------------------------------------- host half */

const host = await import('../lib/index.js')
assert.equal(host.name, 'dsh-winsync')
assert.deepEqual(host.inject, ['webServer'])

let route
const disposers = []
host.apply({
  effect(factory) {
    const dispose = factory()
    disposers.push(dispose)
    return () => {}
  },
  webServer: {
    register(options) {
      route = options
      return () => {}
    },
  },
})

assert.equal(route?.kind, 'prefix')
assert.equal(route.path, '/winsync/api')

const server = http.createServer((req, res) => route.handler(req, res))
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
const { port } = server.address()

const reply = await new Promise((resolve, reject) => {
  http
    .get({ host: '127.0.0.1', port, path: '/winsync/api/ping' }, (res) => {
      const chunks = []
      res.on('data', (chunk) => chunks.push(chunk))
      res.on('end', () => resolve({ status: res.statusCode, body: JSON.parse(Buffer.concat(chunks).toString('utf8')) }))
    })
    .on('error', reject)
})

assert.equal(reply.status, 200)
assert.equal(reply.body.ok, true)
assert.equal(reply.body.plugin, 'dsh-winsync')
assert.ok(reply.body.runtimeDir.endsWith('dsh-winsync'), 'runtime dir must be plugin-owned, not the exe tree')

const missing = await new Promise((resolve, reject) => {
  http
    .get({ host: '127.0.0.1', port, path: '/winsync/api/nope' }, (res) => {
      res.resume()
      res.on('end', () => resolve(res.statusCode))
    })
    .on('error', reject)
})
assert.equal(missing, 404)

server.close()

console.log('P0 smoke: client contract, host route, and runtime dir isolation all pass')
