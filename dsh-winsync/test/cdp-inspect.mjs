/**
 * P0 diagnostic: read the live Web UI through CDP, so "the tab did not appear"
 * becomes an observation instead of a guess.
 *
 * Usage:
 *   node test/cdp-inspect.mjs <page-url> [debug-port]
 *
 * It launches headless Edge/Chrome with remote debugging, loads the page, and
 * reports:
 *   - console errors and uncaught exceptions (why a client plugin failed)
 *   - the boot-graph row for dsh-winsync
 *   - every rendered view tab label (the tab row the user should see)
 *   - a screenshot at test/cdp-shot.png for eyeballing
 *
 * Requires Edge or Chrome. No npm dependencies: CDP is spoken over the built-in
 * WebSocket (Node >= 22).
 */

import { spawn } from 'node:child_process'
import { existsSync, mkdirSync, writeFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const pageUrl = process.argv[2]
const debugPort = Number(process.argv[3] ?? 9223)
// --click=<text>: click the first element whose text contains <text> after load.
// Repeatable, clicked in order: conversation views are session-scoped, so the tab
// row only exists once a session is open, and the tab itself must be selected to
// see its panel.
const clickTexts = process.argv
  .slice(4)
  .filter((a) => a.startsWith('--click='))
  .map((a) => a.slice('--click='.length))
// --aria=<label>: click by accessible name. Sidebar panel entries are icon-only
// buttons, so they carry a label but no text.
const ariaClicks = process.argv
  .slice(4)
  .filter((a) => a.startsWith('--aria='))
  .map((a) => a.slice('--aria='.length))
if (!pageUrl) {
  console.error('usage: node test/cdp-inspect.mjs <page-url> [debug-port] [--click=<text>]...')
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

const profileDir = resolve(dirname(fileURLToPath(import.meta.url)), '.cdp-profile')
mkdirSync(profileDir, { recursive: true })

const child = spawn(
  browser,
  [
    '--headless=new',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    `--remote-debugging-port=${debugPort}`,
    `--user-data-dir=${profileDir}`,
    '--window-size=1440,960',
    pageUrl,
  ],
  { stdio: 'ignore', windowsHide: true },
)

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function targets() {
  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      const response = await fetch(`http://127.0.0.1:${debugPort}/json/list`)
      const list = await response.json()
      const page = list.find((t) => t.type === 'page' && !t.url.startsWith('about:'))
      if (page?.webSocketDebuggerUrl) return page
    } catch {
      /* browser still starting */
    }
    await sleep(250)
  }
  throw new Error('CDP target never appeared')
}

const target = await targets()
const socket = new WebSocket(target.webSocketDebuggerUrl)
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

function send(method, params = {}) {
  const id = nextId++
  socket.send(JSON.stringify({ id, method, params }))
  return new Promise((ok, fail) => pending.set(id, { ok, fail }))
}

await send('Runtime.enable')
await send('Log.enable').catch(() => {})
await send('Page.enable')
await sleep(6000)

const evaluate = async (expression) => {
  const result = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
  return result.result?.value
}

for (const clickText of clickTexts) {
  const clicked = await evaluate(`(() => {
    const wanted = ${JSON.stringify(clickText)};
    const nodes = Array.from(document.querySelectorAll('a, button, [role="button"], [role="tab"], [role="link"], li, div'));
    const hit = nodes
      .filter((el) => (el.innerText || '').includes(wanted) && el.offsetParent !== null)
      .sort((a, b) => (a.innerText || '').length - (b.innerText || '').length)[0];
    if (!hit) return null;
    hit.scrollIntoView({ block: 'center' });
    hit.click();
    return (hit.innerText || '').trim().slice(0, 40);
  })()`)
  console.log(JSON.stringify({ clicked }))
  await sleep(4000)
}

for (const ariaLabel of ariaClicks) {
  const clicked = await evaluate(`(() => {
    const wanted = ${JSON.stringify(ariaLabel)};
    const hit = Array.from(document.querySelectorAll('button, [role="button"], a'))
      .find((el) => ((el.getAttribute('aria-label') || el.title || '').includes(wanted)) && el.offsetParent !== null);
    if (!hit) return null;
    hit.click();
    return hit.getAttribute('aria-label');
  })()`)
  console.log(JSON.stringify({ ariaClicked: ariaLabel, hit: clicked }))
  await sleep(3000)
}

const report = await evaluate(`(() => {
  const boot = window.__DSH_BOOT__;
  const rows = boot?.entries?.filter((e) => String(e.id).includes('winsync')) ?? [];
  const text = document.body.innerText || '';
  const tabLike = Array.from(document.querySelectorAll('[role="tab"], button, [data-slot]'))
    .map((el) => (el.innerText || '').trim())
    .filter((s) => s.length > 0 && s.length < 24);
  return {
    title: document.title,
    bootRow: rows,
    hasWinsyncText: text.includes('同步') || text.includes('WinSync'),
    bodyHead: text.slice(0, 400),
    candidates: Array.from(new Set(tabLike)).slice(0, 40),
    ariaLabels: Array.from(new Set(Array.from(document.querySelectorAll('button, [role="button"], a'))
      .map((el) => (el.getAttribute('aria-label') || '').trim())
      .filter((s) => s.length > 0))).slice(0, 40),
  };
})()`)

const screenshot = await send('Page.captureScreenshot', { format: 'png' })
const shotPath = resolve(dirname(fileURLToPath(import.meta.url)), 'cdp-shot.png')
writeFileSync(shotPath, Buffer.from(screenshot.data, 'base64'))

console.log(JSON.stringify({ ...report, console: consoleLines.slice(-25), screenshot: shotPath }, null, 2))

socket.close()
child.kill()
