/**
 * P3 视觉走查：在真实 dsh 页面上把「同步」面板从空态走到有任务，逐步截图。
 *
 * 用法：
 *   node test/cdp-p3.mjs "http://127.0.0.1:<port>/?token=<token>" [debug-port]
 *
 * 产出 test/shots/p3-*.png。点击一律走 CDP 真实鼠标事件（DOM 的 .click() 会点到
 * innerText 相同的父 div 上，什么也不会发生）。
 */

import { spawn } from 'node:child_process'
import { existsSync, mkdirSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const pageUrl = process.argv[2]
const debugPort = Number(process.argv[3] ?? 9334)
if (!pageUrl) {
  console.error('usage: node test/cdp-p3.mjs <page-url> [debug-port]')
  process.exit(2)
}

const BROWSERS = [
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Google/Chrome/Application/chrome.exe',
]
const browser = BROWSERS.find((candidate) => existsSync(candidate))
if (!browser) {
  console.error('no Edge/Chrome found')
  process.exit(2)
}

const here = dirname(fileURLToPath(import.meta.url))
const shots = resolve(here, 'shots')
mkdirSync(shots, { recursive: true })

const root = join(tmpdir(), 'wsd-p3-root')
mkdirSync(join(root, 'Docs'), { recursive: true })
mkdirSync(join(root, 'Pics'), { recursive: true })

// 走查要可重复：先把上一个任务删掉，否则每次跑都会多一张卡
const apiBase = new URL(pageUrl).origin + '/winsync/api'
const existing = await (await fetch(`${apiBase}/state`)).json()
for (const task of existing.tasks ?? []) {
  const reply = await fetch(`${apiBase}/tasks/${task.id}`, { method: 'DELETE' })
  if (!reply.ok) throw new Error(`清理旧任务失败：${task.id} ${reply.status}`)
}
if ((existing.tasks ?? []).length > 0) console.log(`已清理 ${existing.tasks.length} 个旧任务`)

const child = spawn(
  browser,
  [
    '--headless=new',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    `--remote-debugging-port=${debugPort}`,
    `--user-data-dir=${resolve(here, '.cdp-profile')}`,
    `--window-size=${process.env.CDP_SIZE ?? '1440,960'}`,
    pageUrl,
  ],
  { stdio: 'ignore', windowsHide: true },
)

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function target() {
  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json()
      const page = list.find((t) => t.type === 'page' && !t.url.startsWith('about:'))
      if (page?.webSocketDebuggerUrl) return page
    } catch {
      /* still starting */
    }
    await sleep(250)
  }
  throw new Error('CDP target never appeared')
}

const socket = new WebSocket((await target()).webSocketDebuggerUrl)
await new Promise((ok, fail) => {
  socket.addEventListener('open', ok, { once: true })
  socket.addEventListener('error', fail, { once: true })
})

let nextId = 1
const pending = new Map()
const consoleLines = []
socket.addEventListener('message', (event) => {
  const message = JSON.parse(event.data)
  if (message.id !== undefined) {
    const entry = pending.get(message.id)
    if (entry) {
      pending.delete(message.id)
      message.error ? entry.fail(new Error(JSON.stringify(message.error))) : entry.ok(message.result)
    }
    return
  }
  if (message.method === 'Runtime.consoleAPICalled') {
    const { type, args } = message.params
    if (type === 'error' || type === 'warning') {
      consoleLines.push(`${type}: ${args.map((a) => a.value ?? a.description ?? a.type).join(' ')}`)
    }
  }
  if (message.method === 'Runtime.exceptionThrown') {
    const d = message.params.exceptionDetails
    consoleLines.push(`exception: ${d.exception?.description ?? d.text}`)
  }
})

const send = (method, params = {}) => {
  const id = nextId++
  socket.send(JSON.stringify({ id, method, params }))
  return new Promise((ok, fail) => pending.set(id, { ok, fail }))
}

await send('Runtime.enable')
await send('Page.enable')
await sleep(6000)

const evaluate = async (expression) => {
  const result = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
  if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails))
  return result.result?.value
}

async function shot(name) {
  const png = await send('Page.captureScreenshot', { format: 'png' })
  const file = resolve(shots, `p3-${name}.png`)
  writeFileSync(file, Buffer.from(png.data, 'base64'))
  return file
}

/* ------------------------------------------------------------ 交互原语 */

/** 找到满足条件的元素中心点（可见、最内层）。 */
const centerOf = (finder) =>
  evaluate(`(() => {
    const hit = (${finder});
    if (!hit) return null;
    const r = hit.getBoundingClientRect();
    if (!r.width || !r.height) return null;
    return { x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2), text: (hit.innerText || hit.value || '').trim().slice(0, 40) };
  })()`)

async function mouseClick(point) {
  for (const type of ['mousePressed', 'mouseReleased']) {
    await send('Input.dispatchMouseEvent', {
      type,
      x: point.x,
      y: point.y,
      button: 'left',
      clickCount: 1,
      buttons: type === 'mousePressed' ? 1 : 0,
    })
  }
  await sleep(700)
}

/** 按可见文本点按钮（只要 button/[role=button]/a，避免点到同名父 div）。 */
async function clickButton(text, { exact = false } = {}) {
  const point = await centerOf(`(() => {
    const wanted = ${JSON.stringify(text)};
    const nodes = Array.from(document.querySelectorAll('button, [role="button"], a'));
    return nodes
      .filter((el) => {
        const t = (el.innerText || '').trim();
        return (${exact} ? t === wanted : t.includes(wanted)) && el.offsetParent !== null && !el.disabled;
      })
      .sort((a, b) => (a.innerText || '').length - (b.innerText || '').length)[0] || null;
  })()`)
  if (!point) throw new Error(`找不到按钮：${text}`)
  await mouseClick(point)
  return point.text
}

async function clickAria(label) {
  const point = await centerOf(`(() => {
    const wanted = ${JSON.stringify(label)};
    return Array.from(document.querySelectorAll('button, [role="button"], a'))
      .find((el) => ((el.getAttribute('aria-label') || el.title || '') === wanted) && el.offsetParent !== null) || null;
  })()`)
  if (!point) throw new Error(`找不到 aria：${label}`)
  await mouseClick(point)
  return point.text
}

/** 受控输入必须走原生 setter + input 事件，React 才认。 */
const fill = (label, value) =>
  evaluate(`(() => {
    const wanted = ${JSON.stringify(label)};
    const field = Array.from(document.querySelectorAll('.ws-field'))
      .find((el) => ((el.querySelector('label') || {}).innerText || '').trim() === wanted);
    if (!field) return 'no-field';
    const input = field.querySelector('input, textarea');
    if (!input) return 'no-input';
    const proto = input instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(input, ${JSON.stringify(value)});
    input.dispatchEvent(new Event('input', { bubbles: true }));
    return input.value;
  })()`)

const panelText = () =>
  evaluate(`(() => {
    const parts = [];
    const root = document.querySelector('.ws-root');
    if (root) parts.push(root.innerText);
    for (const modal of document.querySelectorAll('.ws-modal, .ws-raw')) parts.push(modal.innerText);
    return parts.join('\\n---modal---\\n').replace(/\\n{2,}/g, '\\n').slice(0, 900);
  })()`)

const steps = []
const shotStep = async (name, note) => {
  steps.push({ step: name, note, file: await shot(name), text: await panelText() })
}

/* ------------------------------------------------------------ 走一遍 */

await clickAria('同步')
await sleep(2000)
// dsh 自己的预览版说明会盖住面板
await clickButton('继续', { exact: true }).catch(() => null)
await shotStep('01-empty', '空态')

await clickButton('创建同步任务')
await sleep(600)
await fill('服务器地址', 'ws://127.0.0.1:18443/ws')
await fill('密钥', 'p3-demo-token')
await fill('房间名', 'default')
await sleep(300)
await shotStep('02-wizard-server', '向导 1/3 服务器')

await clickButton('下一步')
await fill('本机同步目录', root)
await sleep(300)
await shotStep('03-wizard-dir', '向导 2/3 目录')

await clickButton('下一步')
await sleep(300)
await shotStep('04-wizard-confirm', '向导 3/3 确认')

await clickButton('创建')
await sleep(2500)
await shotStep('05-task-card', '任务卡')

await clickButton('同步日志', { exact: true })
await sleep(800)
await shotStep('06-logs', '日志页')

await clickButton('设置', { exact: true })
await sleep(800)
await shotStep('07-settings', '设置页')

/* 容器查询：面板宽度由 dsh 分栏决定，直接改 .ws-root 宽度来验证断点 */
await clickButton('同步任务', { exact: true })
await sleep(600)
const widths = []
for (const px of [900, 780, 600]) {
  widths.push(
    await evaluate(`(() => {
      const root = document.querySelector('.ws-root');
      if (!root) return { px: ${px}, error: 'no-root' };
      root.style.width = ${px} + 'px';
      root.style.maxWidth = ${px} + 'px';
      const info = document.querySelector('.ws-info');
      const side = document.querySelector('.ws-side');
      return {
        px: ${px},
        rootWidth: Math.round(root.getBoundingClientRect().width),
        columns: info ? getComputedStyle(info).gridTemplateColumns : null,
        sideWidth: side ? Math.round(side.getBoundingClientRect().width) : null,
        brandHidden: document.querySelector('.ws-brand-name')
          ? getComputedStyle(document.querySelector('.ws-brand-name')).display === 'none'
          : null,
      };
    })()`),
  )
  await sleep(400)
}
await shotStep('08-narrow', '面板 600px 宽')
await evaluate(`(() => {
  const root = document.querySelector('.ws-root');
  if (root) { root.style.width = ''; root.style.maxWidth = ''; }
})()`)

console.log(JSON.stringify({ root, widths, steps, console: consoleLines.slice(-20) }, null, 2))

socket.close()
child.kill()
