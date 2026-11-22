/**
 * client smoke — no dsh, no browser, no Rust.
 *
 * 它复现客户端那一条契约，让回归在这里被抓住，而不是靠重启桌面端才发现:
 *
 *   browser half: the bundle registers its factory through
 *      `window.__ModuleLoader__.load({ id, factory })`, the factory returns a
 *      Cordis client plugin, and `apply(ctx)` registers the `main` panel plus
 *      the `sidebar.panellist` row that opens it, both under one id.
 *
 * Run: node test/client-smoke.mjs
 */

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

/* ---------------------------------------------------------------- browser half */

const bundle = readFileSync(new URL('../lib/client.js', import.meta.url), 'utf8')

let captured
const fakeReact = {
  createElement: (type, props, ...children) => ({ type, props, children }),
  Fragment: Symbol('Fragment'),
  useState: (initial) => [typeof initial === 'function' ? initial() : initial, () => {}],
  useEffect: () => {},
  useCallback: (fn) => fn,
  useRef: (initial) => ({ current: initial }),
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
// layout 是「返回对话」用的宿主服务：面板必须能自己交还主区。
assert.deepEqual(clientPlugin.inject, ['slots', 'layout'])

const registrations = []
const selected = []
clientPlugin.apply({
  layout: {
    selectPanel(panelId) {
      selected.push(panelId)
    },
  },
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

// 主区面板接管了整块画布，必须自带出口：onBack 交还布局给会话。
assert.equal(typeof tree.props?.onBack, 'function', 'panel shell must expose a way back to dsh')
tree.props.onBack()
assert.deepEqual(selected, [null], 'back must clear the active panel (null = conversation)')

const glyph = row.component({ size: 20, active: false })
assert.equal(glyph.type, 'svg')
assert.equal(glyph.props.width, 20)
assert.equal(glyph.props.height, 20)

console.log("client smoke: 注册契约通过")
