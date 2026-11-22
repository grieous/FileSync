/**
 * 在真实页面上跑一段表达式，打印结果。调试用。
 *
 *   node test/cdp-eval.mjs "http://127.0.0.1:<port>/?token=..." 9360 "<js>"
 *
 * 表达式里的 await 会被当作顶层 await 处理（包在 async IIFE 里）。
 */
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const [pageUrl, portArg, expression] = process.argv.slice(2)
if (!pageUrl || !expression) {
  console.error('usage: node test/cdp-eval.mjs <page-url> <debug-port> "<js>"')
  process.exit(2)
}
const debugPort = Number(portArg ?? 9360)
const BROWSERS = [
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Google/Chrome/Application/chrome.exe',
]
const browser = BROWSERS.find((c) => existsSync(c))
const here = dirname(fileURLToPath(import.meta.url))

const child = spawn(
  browser,
  [
    '--headless=new',
    '--disable-gpu',
    '--no-first-run',
    `--remote-debugging-port=${debugPort}`,
    `--user-data-dir=${resolve(here, '.cdp-profile')}`,
    '--window-size=1440,960',
    pageUrl,
  ],
  { stdio: 'ignore', windowsHide: true },
)

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

const target = async () => {
  for (let i = 0; i < 60; i += 1) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json()
      const page = list.find((t) => t.type === 'page' && !t.url.startsWith('about:'))
      if (page?.webSocketDebuggerUrl) return page
    } catch {
      /* starting */
    }
    await sleep(250)
  }
  throw new Error('no target')
}

const socket = new WebSocket((await target()).webSocketDebuggerUrl)
await new Promise((ok, fail) => {
  socket.addEventListener('open', ok, { once: true })
  socket.addEventListener('error', fail, { once: true })
})

let id = 1
const pending = new Map()
socket.addEventListener('message', (event) => {
  const message = JSON.parse(event.data)
  if (message.id === undefined) return
  const entry = pending.get(message.id)
  if (!entry) return
  pending.delete(message.id)
  message.error ? entry.fail(new Error(JSON.stringify(message.error))) : entry.ok(message.result)
})
const send = (method, params = {}) => {
  const current = id++
  socket.send(JSON.stringify({ id: current, method, params }))
  return new Promise((ok, fail) => pending.set(current, { ok, fail }))
}

await send('Runtime.enable')
await sleep(6500)
const result = await send('Runtime.evaluate', {
  expression: `(async () => { ${expression} })()`,
  awaitPromise: true,
  returnByValue: true,
})
if (result.exceptionDetails) {
  console.error('EXCEPTION', JSON.stringify(result.exceptionDetails.exception?.description ?? result.exceptionDetails))
} else {
  console.log(JSON.stringify(result.result?.value, null, 2))
}

socket.close()
child.kill()
