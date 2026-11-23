/**
 * 版本角标的界面测试（可选，需要 react + react-dom + jsdom）。
 *
 * 覆盖三件事：
 *   1. `/version` 有回应 → 侧栏底部出现「v<版本> · <commit>」
 *   2. 有未提交改动 → 角标带 `+dirty`，并能点出复制成功的提示
 *   3. `/version` 挂了 → 界面照常可用，只是不显示角标（不能因为一个诊断信息把面板拖崩）
 *
 * 用法（仓库里没有 devDependencies，先借一份 node_modules）：
 *   ln -s <装有 react/react-dom/jsdom 的目录> node_modules
 *   node test/version-ui.mjs
 */

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

import { JSDOM } from 'jsdom'
import React from 'react'
import { createRoot } from 'react-dom/client'

globalThis.IS_REACT_ACT_ENVIRONMENT = true

const BUNDLE = process.env.WINSYNC_CLIENT ?? new URL('../lib/client.js', import.meta.url).pathname

const DOM = '<!doctype html><html><body><div id="app"></div></body></html>'

function setup() {
  const dom = new JSDOM(DOM, { url: 'http://localhost/', pretendToBeVisual: true })
  const { window } = dom
  global.window = window
  global.document = window.document
  Object.defineProperty(global, 'navigator', { value: window.navigator, configurable: true })
  for (const key of ['Event', 'MouseEvent', 'KeyboardEvent', 'InputEvent', 'Element', 'HTMLElement', 'Node', 'MutationObserver', 'getComputedStyle']) {
    global[key] = window[key]
  }
  window.HTMLElement.prototype.focus = () => {}
  // React 的 onChange 靠 oninput 属性探针来判断该不该合成事件
  for (const name of ['oninput', 'onchange']) {
    if (!(name in window.HTMLElement.prototype)) {
      Object.defineProperty(window.HTMLElement.prototype, name, { configurable: true, writable: true, value: null })
    }
  }
  class EventSourceStub {
    addEventListener() {}
    close() {}
  }
  global.EventSource = EventSourceStub
  window.EventSource = EventSourceStub
  return window
}

const SELF = {
  ok: true,
  plugin: 'dsh-winsync',
  version: '0.5.3',
  label: '0.5.3 · c76a83c+dirty',
  commit: 'c76a83c',
  commitSource: 'git',
  branch: 'master',
  dirty: true,
  builtAt: '2026-11-23T00:00:00.000Z',
  dir: 'C:\\repo\\dsh-winsync',
}

const STATE = {
  ok: true,
  version: '0.5',
  device: 'probe',
  auto: true,
  data_dir: 'C:\\state',
  settings: { autostart: false, rate_up: 0, rate_down: 0 },
  tasks: [],
}

/** 装一份 fetch：versionMode 控制 /version 正常、500、还是永不返回。 */
function installFetch(window, versionMode) {
  const copyCalls = []
  let pendingResolve = null
  global.fetch = async (url) => {
    const target = String(url)
    if (target.includes('/state')) {
      return { ok: true, status: 200, text: async () => JSON.stringify(STATE) }
    }
    if (target.includes('/version')) {
      if (versionMode === 'fail') {
        return { ok: false, status: 500, text: async () => JSON.stringify({ ok: false, error: { code: 'boom', message: 'boom' } }) }
      }
      if (versionMode === 'pending') {
        await new Promise((resolve) => {
          pendingResolve = resolve
        })
      }
      return { ok: true, status: 200, text: async () => JSON.stringify(SELF) }
    }
    return { ok: true, status: 200, text: async () => JSON.stringify({ ok: true, rows: [] }) }
  }
  window.fetch = global.fetch

  // jsdom 没有剪贴板权限，这里接住写入，顺便记录内容
  Object.defineProperty(window.navigator, 'clipboard', {
    configurable: true,
    value: {
      writeText: async (text) => {
        copyCalls.push(text)
      },
    },
  })
  return { copyCalls, flush: () => pendingResolve?.() }
}

async function mount(window) {
  let captured
  window.__ModuleLoader__ = { load: (options) => (captured = options) }
  // client.js 是给宿主 loader 的工厂包，这里按同样协议求值
  new Function('window', readFileSync(BUNDLE, 'utf8'))(window)
  const plugin = captured.factory(() => React)
  const registered = []
  plugin.apply({
    layout: { selectPanel() {} },
    slots: {
      inject: (_key, callback) => callback(),
      register: (options, component) => {
        registered.push({ options, component })
        return () => {}
      },
    },
  })
  const Page = registered.find((entry) => entry.options.name === 'main').component
  const root = createRoot(document.getElementById('app'))
  await React.act(async () => {
    root.render(React.createElement(Page, {}))
    await new Promise((resolve) => setTimeout(resolve, 20))
  })
  return root
}

const text = () => document.body.textContent ?? ''

/* ------------------------------------------------------------ 1. 正常显示 */

{
  const window = setup()
  installFetch(window, 'ok')
  const root = await mount(window)

  const badge = document.querySelector('.ws-ver')
  assert.ok(badge, '没有渲染出版本角标')
  assert.equal(badge.textContent, 'v0.5.3 · c76a83c+dirty', `角标文案不对：${badge.textContent}`)
  assert.equal(badge.dataset.dirty, 'true', 'dirty 状态没有反映到样式钩子上')
  assert.match(badge.getAttribute('title'), /c76a83c/, 'title 里没有 commit')
  assert.match(badge.getAttribute('title'), /有未提交改动/, 'title 里没提未提交改动')
  console.log('PASS 角标显示版本 + commit + dirty')

  // 点击复制：粘到 issue 里的那份文本要含版本、提交、目录
  await React.act(async () => {
    badge.dispatchEvent(new window.MouseEvent('click', { bubbles: true }))
    await new Promise((resolve) => setTimeout(resolve, 10))
  })
  const copied = document.querySelector('.ws-toast')
  assert.ok(copied, '复制后没有提示')
  assert.match(text(), /版本信息已复制/, '提示文案不对')
  console.log('PASS 点击角标会复制版本信息并提示')

  await React.act(async () => root.unmount())
}

/* ------------------------------------------------------------ 2. 接口挂了 */

{
  setup()
  installFetch(global.window, 'fail')
  await mount(global.window)
  assert.equal(document.querySelector('.ws-ver'), null, '/version 失败时不该渲染角标')
  assert.match(text(), /同步任务/, '版本接口失败不该把面板拖垮')
  console.log('PASS /version 失败时面板照常可用，只是不显示角标')
}

/* ------------------------------------------------------------ 3. 还没回来 */

{
  setup()
  installFetch(global.window, 'pending')
  await mount(global.window)
  assert.equal(document.querySelector('.ws-ver'), null, '请求未完成时不该占位渲染')
  assert.match(text(), /同步任务/, '请求未完成时面板应当正常显示')
  console.log('PASS /version 未返回时不占位、不阻塞')
}

console.log('version-ui: 全部通过')
