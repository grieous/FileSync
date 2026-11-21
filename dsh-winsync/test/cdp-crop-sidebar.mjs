import { spawn } from 'node:child_process'
import fs from 'node:fs'

const url = process.argv[2]
const port = process.argv[3]
const browser = 'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe'
const child = spawn(browser, [
  '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
  `--remote-debugging-port=${port}`,
  `--user-data-dir=C:/Users/gjn86/AppData/Local/Temp/cdpprof${port}`,
  '--window-size=1440,960', url,
], { stdio: 'ignore', windowsHide: true })

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let page
for (let i = 0; i < 60; i += 1) {
  try {
    const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()
    page = list.find((t) => t.type === 'page' && !t.url.startsWith('about:'))
    if (page?.webSocketDebuggerUrl) break
  } catch {}
  await sleep(250)
}

const ws = new WebSocket(page.webSocketDebuggerUrl)
await new Promise((ok, f) => { ws.addEventListener('open', ok, { once: true }); ws.addEventListener('error', f, { once: true }) })
let id = 1
const pending = new Map()
ws.addEventListener('message', (e) => {
  const m = JSON.parse(e.data)
  if (m.id) { const x = pending.get(m.id); if (x) { pending.delete(m.id); m.error ? x.f(new Error(JSON.stringify(m.error))) : x.o(m.result) } }
})
const send = (method, params = {}) => { const i = id++; ws.send(JSON.stringify({ id: i, method, params })); return new Promise((o, f) => pending.set(i, { o, f })) }

await send('Runtime.enable')
await send('Page.enable')
await sleep(7000)

const clickExpr = [
  '(() => {',
  "  const b = Array.from(document.querySelectorAll('button')).find((x) => (x.getAttribute('aria-label') || '') === '同步');",
  '  if (!b) return false;',
  '  b.click();',
  '  return true;',
  '})()',
].join('\n')
await send('Runtime.evaluate', { expression: clickExpr, returnByValue: true })
await sleep(2500)

const boxExpr = [
  '(() => {',
  "  const b = Array.from(document.querySelectorAll('button')).find((x) => (x.getAttribute('aria-label') || '') === '同步');",
  '  if (!b) return null;',
  '  const r = b.getBoundingClientRect();',
  "  const s = b.querySelector('svg');",
  '  const cs = s ? getComputedStyle(s) : null;',
  '  return { x: r.x, y: r.y, w: r.width, h: r.height, color: cs?.color, opacity: cs?.opacity, display: cs?.display };',
  '})()',
].join('\n')
const box = await send('Runtime.evaluate', { expression: boxExpr, returnByValue: true })
const r = box.result.value
console.log(JSON.stringify(r))

if (r) {
  const shot = await send('Page.captureScreenshot', {
    format: 'png',
    clip: { x: 0, y: Math.max(0, r.y - 60), width: 280, height: 170, scale: 4 },
  })
  const out = 'C:/Users/gjn86/WorkBuddy/spaces/2026_11_19_20-38-45/dsh-winsync/test/cdp-sidebar-crop.png'
  fs.writeFileSync(out, Buffer.from(shot.data, 'base64'))
  console.log(out)
}

ws.close()
child.kill()
