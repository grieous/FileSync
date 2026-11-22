/**
 * 只看不改：连上真实页面，把「同步」面板的每一处界面截下来，用于人工验收。
 *
 *   node test/cdp-view.mjs "http://127.0.0.1:<port>/?token=<token>" [debug-port] [标签页名]
 *
 * 与 cdp-p3.mjs 的区别：不创建、不删除任何任务，只读现有数据。
 * 产出 test/shots/view-*.png。
 */
import { spawn } from 'node:child_process'
import { existsSync, mkdirSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const pageUrl = process.argv[2]
const debugPort = Number(process.argv[3] ?? 9350)
if (!pageUrl) {
  console.error('usage: node test/cdp-view.mjs <page-url> [debug-port]')
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

// 每次一个新 profile：复用同一个目录时 Edge 会把新请求转给旧实例，
// /json/list 拿到的可能是上一次遗留的页面（表现为面板打开后数据是空的）。
const profileDir = resolve(here, `.cdp-profile-${debugPort}`)
rmSync(profileDir, { recursive: true, force: true })

const child = spawn(
  browser,
  [
    '--headless=new',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    `--remote-debugging-port=${debugPort}`,
    `--user-data-dir=${profileDir}`,
    `--window-size=${process.env.CDP_SIZE ?? '1440,960'}`,
    pageUrl,
  ],
  { stdio: 'ignore', windowsHide: true },
)

const cleanup = () => {
  try {
    child.kill()
  } catch {
    /* already gone */
  }
}
process.on('exit', cleanup)
process.on('uncaughtException', (error) => {
  console.error(error)
  cleanup()
  process.exit(1)
})

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

const shot = async (name) => {
  const png = await send('Page.captureScreenshot', { format: 'png' })
  const file = resolve(shots, `view-${name}.png`)
  writeFileSync(file, Buffer.from(png.data, 'base64'))
  return file
}

/** 按元素框裁剪放大，用于看图标这类小细节。必须放在最后调用（会让面板重排）。 */
const shotElement = async (name, selector, index = 0, scale = 6) => {
  const clip = await evaluate(`(() => {
    const el = document.querySelectorAll(${JSON.stringify(selector)})[${index}];
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: Math.floor(r.x) - 6, y: Math.floor(r.y) - 6, width: Math.ceil(r.width) + 12, height: Math.ceil(r.height) + 12 };
  })()`)
  if (!clip) return null
  const png = await send('Page.captureScreenshot', { format: 'png', clip: { ...clip, scale } })
  const file = resolve(shots, `view-${name}.png`)
  writeFileSync(file, Buffer.from(png.data, 'base64'))
  return file
}

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

async function clickButton(text, { exact = false, index = 0 } = {}) {
  const hit = await evaluate(`(() => {
    const wanted = ${JSON.stringify(text)};
    const nodes = Array.from(document.querySelectorAll('button, [role="button"], a'))
      .filter((el) => {
        const t = (el.innerText || '').trim();
        return (${exact} ? t === wanted : t.includes(wanted)) && el.getClientRects().length > 0 && !el.disabled;
      })
      .sort((a, b) => (a.innerText || '').length - (b.innerText || '').length);
    const target = nodes[${index}];
    if (!target) return null;
    const text = (target.innerText || '').trim().slice(0, 40);
    target.click();
    return text;
  })()`)
  if (!hit) throw new Error(`找不到按钮：${text}`)
  await sleep(700)
  return hit
}

async function clickAria(label) {
  const hit = await evaluate(`(() => {
    const wanted = ${JSON.stringify(label)};
    const target = Array.from(document.querySelectorAll('button, [role="button"], a'))
      .find((el) => (el.getAttribute('aria-label') || el.getAttribute('title') || '') === wanted && el.getClientRects().length > 0);
    if (!target) return null;
    target.click();
    return (target.innerText || '').trim().slice(0, 40) || wanted;
  })()`)
  if (!hit) throw new Error(`找不到 aria：${label}`)
  await sleep(700)
  return hit
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

/** 等任务卡列表重新渲染出来（重挂后 /state 回来之前是空态）。 */
async function waitCards(timeoutMs = 8000) {
  const until = Date.now() + timeoutMs
  while (Date.now() < until) {
    const count = await evaluate(`document.querySelectorAll('.ws-root .ws-icon-btn').length`)
    if (count > 0) return count
    await sleep(250)
  }
  return 0
}

/** 点第 n 张任务卡右上角的 ⋯（title=更多）。 */
const clickMore = async (n) => {
  await waitCards()
  let hit = await evaluate(`(() => {
    const buttons = Array.from(document.querySelectorAll('.ws-icon-btn'))
      .filter((el) => (el.getAttribute('title') || '') === '更多' && el.getClientRects().length > 0);
    const target = buttons[${n}];
    if (!target) return {
      ok: false,
      roots: Array.from(document.querySelectorAll('.ws-root')).map((r) => ({
        w: r.offsetWidth,
        h: r.offsetHeight,
        vis: r.getClientRects().length,
        btns: r.querySelectorAll('.ws-icon-btn').length,
        kids: Array.from(r.children).map((c) => c.className).slice(0, 4),
        textLen: (r.innerText || '').length,
      })),
      url: location.href.slice(0, 60),
      hasTaskName: (document.body.innerText || '').includes('工作文档'),
    };
    target.click();
    return { ok: true };
  })()`)
  if (!hit?.ok) {
    console.error('console:', consoleLines.slice(-8))
    throw new Error(`找不到第 ${n} 张卡的更多按钮：${JSON.stringify(hit)}`)
  }
  await sleep(600)
}

const panelText = () =>
  evaluate(`(() => {
    const parts = [];
    const root = document.querySelector('.ws-root');
    if (root) parts.push(root.innerText);
    for (const modal of document.querySelectorAll('.ws-modal, .ws-raw')) parts.push(modal.innerText);
    return parts.join('\\n---modal---\\n').replace(/\\n{2,}/g, '\\n').slice(0, 1200);
  })()`)

/** 打开面板并等它稳定渲染（重复运行会残留选中态，直接点反而会关掉）。 */
async function openPanel() {
  // dsh 认证后会 location 换掉 ?token=...，中途页面会重挂一次；不等就会撞在空态上
  for (let i = 0; i < 40; i += 1) {
    const search = await evaluate(`location.search`)
    if (!String(search).includes('token')) break
    await sleep(300)
  }
  await sleep(1500)

  const opened = () => evaluate(`(() => {
    const root = document.querySelector('.ws-root');
    if (!root) return { root: false, cards: 0, iconBtns: 0 };
    return {
      root: true,
      cards: root.querySelectorAll('.ws-card').length,
      iconBtns: root.querySelectorAll('.ws-icon-btn').length,
      text: (root.innerText || '').slice(0, 40),
    };
  })()`)

  let state = await opened()
  if (!state.root) {
    await clickAria('同步')
    await sleep(1200)
    state = await opened()
  }
  // 面板在但列表空了：dsh 重排后面板会被重挂，这里点一下重新拉起
  for (let round = 0; round < 2 && state.root && state.iconBtns === 0; round += 1) {
    for (let i = 0; i < 15 && state.iconBtns === 0; i += 1) {
      await sleep(300)
      state = await opened()
    }
    if (state.iconBtns === 0) {
      console.log('列表空态，重新打开面板')
      await clickAria('同步')
      await sleep(1500)
      state = await opened()
    }
  }
  return state
}

/* ------------------------------------------------------------------ 走一遍 */

// 一次只做一步：截图这个动作本身会让 dsh 重排面板（数据被清），
// 连续截多张时后面的步骤全在空态上跑，所以每个场景单独起一次浏览器。
const only = (process.argv[4] ?? 'tasks').replace(/^--only=/, '')

const panel = await openPanel()
console.log('面板状态', JSON.stringify(panel))

const steps = {
  tasks: async () => {},
  menu: async () => {
    await clickMore(0)
  },
  editor: async () => {
    await clickMore(0)
    await sleep(300)
    // 必须点菜单里的「设置」，侧栏也有同名导航项
    await evaluate(`(() => {
      const item = Array.from(document.querySelectorAll('.ws-menu button')).find((b) => b.innerText.trim() === '设置');
      if (item) item.click();
    })()`)
  },
  raw: async () => {
    await clickMore(0)
    await sleep(300)
    await evaluate(`(() => {
      const item = Array.from(document.querySelectorAll('.ws-menu button')).find((b) => b.innerText.trim() === '查看日志');
      if (item) item.click();
    })()`)
  },
  logs: async () => {
    await clickButton('同步日志', { exact: true })
  },
  settings: async () => {
    await clickButton('设置', { exact: true })
  },
  narrow: async () => {
    await evaluate(`(() => {
      const root = document.querySelector('.ws-root');
      if (root) { root.style.width = '620px'; root.style.maxWidth = '620px'; }
    })()`)
  },
  empty: async () => {
    await clickButton('同步日志', { exact: true })
  },
  wizard: async () => {
    await clickButton('创建同步任务')
    await sleep(700)
  },
  wizard3: async () => {
    await clickButton('创建同步任务')
    await sleep(700)
    await fill('服务器地址', 'ws://127.0.0.1:18443/ws')
    await fill('密钥', 'demo-token-2026')
    await fill('房间名', 'preview')
    await clickButton('下一步')
    await sleep(400)
    await fill('本机同步目录', 'C:\\Users\\gjn86\\AppData\\Local\\Temp\\winsync-demo\\code')
    await clickButton('下一步')
    await sleep(400)
  },
}

const step = steps[only]
if (!step) {
  console.error(`未知步骤：${only}（可选：${Object.keys(steps).join(', ')}）`)
  process.exit(2)
}
await step()
await sleep(900)

const text = await panelText()
console.log(JSON.stringify({ only, text, console: consoleLines.slice(-10) }, null, 2))
await shot(only)
if (only === 'tasks') {
  await shotElement('tasks-icon', '.ws-root .ws-card .ws-folder', 0, 8)
  await shotElement('tasks-actions', '.ws-root .ws-card .ws-card-actions', 0, 4)
}

socket.close()
child.kill()
