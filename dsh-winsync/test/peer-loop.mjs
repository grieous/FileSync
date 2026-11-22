/**
 * 演示用：让一个 CLI 对端常驻在 relay 上，好让 daemon 侧的自动同步能真正跑通。
 *
 *   node test/peer-loop.mjs <workspace> <root> [minutes]
 *
 * 每 6 秒跑一次一次性 sync，退出后立刻补上，等价于「对面那台机器一直开着」。
 */
import { spawn } from 'node:child_process'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const [workspace = 'office', rootArg = '', minutesArg = '10'] = process.argv.slice(2)
const root = rootArg || path.join(process.env.TEMP ?? '/tmp', 'winsync-peer', workspace)
const deadline = Date.now() + Number(minutesArg) * 60_000
const bin = path.resolve(here, '../../sync/target/release/sync-client.exe')

const runOnce = () =>
  new Promise((resolve) => {
    const child = spawn(bin, ['--force', '--wait', '20', 'sync'], {
      env: {
        ...process.env,
        WINSYNC_STATE_DIR: path.join(process.env.TEMP ?? '/tmp', 'winsync-peer', `state-${workspace}`),
        SYNC_SERVER: 'ws://127.0.0.1:18443/ws',
        SYNC_TOKEN: 'demo-token-2026',
        SYNC_WORKSPACE: workspace,
        SYNC_ROOT: root,
      },
      windowsHide: true,
    })
    let out = ''
    child.stdout.on('data', (d) => (out += d))
    child.stderr.on('data', (d) => (out += d))
    child.on('exit', () => resolve(out.trim().split('\n').slice(-3).join(' | ')))
  })

while (Date.now() < deadline) {
  const summary = await runOnce()
  console.log(`[${new Date().toLocaleTimeString()}] ${workspace} ${summary}`)
  await new Promise((r) => setTimeout(r, 6000))
}
