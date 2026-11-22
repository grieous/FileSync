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
// 面板不再自带「返回 dsh」按钮，只依赖 slots（宿主左侧栏点会话即可离开）。
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
assert.equal(row.options.label(), '文件同步')
assert.equal(typeof row.component, 'function')

// Both components must survive a bare call: the panel shell renders, and the
// sidebar glyph draws at the size the sidebar passes (hooks are no-ops here).
const tree = page.component({})
assert.ok(tree && typeof tree === 'object', 'WinsyncPage returned nothing')

const glyph = row.component({ size: 20, active: false })
assert.equal(glyph.type, 'svg')
assert.equal(glyph.props.width, 20)
assert.equal(glyph.props.height, 20)
assert.equal(glyph.props.stroke, 'currentColor', 'icon must inherit sidebar color, not a hard-coded blue')
const fallback = row.component({})
assert.equal(fallback.props.width, 18, 'default icon size matches dsh sidebar icons')

console.log("client smoke: 注册契约通过")
