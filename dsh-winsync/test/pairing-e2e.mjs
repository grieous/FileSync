/**
 * 配对的端到端验证：真中继 + 三端，验证「进房即配对」和旁路房间的隔离。
 *
 * 前面的 test/pairing.mjs 用假中继测界面状态机，这里补上真协议这一侧：
 *   1. 两端进 `<房间>::pair` 时，彼此都收到 peer_online=true（谁先谁后都行）
 *   2. 一端离开，另一端收到 peer_online=false
 *   3. `<房间>::pair` 与真房间 `<房间>` 互不干扰——真房间里的 daemon 不会被 UI 的等待连接
 *      挤掉，也不会被当成对端
 *   4. 同 device_id 重连是替换自己，不是占第二个位置
 *
 * 用法：先起中继（SYNC_ADDR=127.0.0.1:18443 cargo run -p sync-server），然后
 *   node test/pairing-e2e.mjs [ws://127.0.0.1:18443/ws]
 */

import assert from 'node:assert/strict'

const URL = process.argv[2] ?? process.env.RELAY_URL ?? 'ws://127.0.0.1:18443/ws'

const encode = (message) => {
  const body = new TextEncoder().encode(JSON.stringify(message))
  const frame = new Uint8Array(1 + body.length)
  frame[0] = 0
  frame.set(body, 1)
  return frame
}
const decode = (data) => JSON.parse(new TextDecoder().decode(new Uint8Array(data).subarray(1)))

/** 一条连接到中继的「客户端」，按房间进，收到的控制帧都记下来。 */
class Peer {
  constructor(room, device) {
    this.room = room
    this.device = device
    this.frames = []
    this.waiters = []
    this.socket = new WebSocket(URL)
    this.socket.binaryType = 'arraybuffer'
    this.ready = new Promise((resolve, reject) => {
      this.socket.addEventListener('open', () => {
        this.socket.send(encode({ t: 'hello', device_id: device, workspace_id: room, proto_ver: 1 }))
        resolve()
      })
      this.socket.addEventListener('error', () => reject(new Error(`连不上 ${URL}`)))
    })
    this.socket.addEventListener('message', (event) => {
      const message = decode(event.data)
      this.frames.push(message)
      for (const waiter of [...this.waiters]) {
        if (waiter.match(message)) {
          this.waiters.splice(this.waiters.indexOf(waiter), 1)
          waiter.resolve(message)
        }
      }
    })
  }

  /** 等一条满足条件的消息（已经收到过的也算）。 */
  async next(match, timeoutMs = 3000) {
    const found = this.frames.find(match)
    if (found) return found
    return new Promise((resolve, reject) => {
      const waiter = { match, resolve: (message) => { clearTimeout(timer); resolve(message) } }
      const timer = setTimeout(() => {
        this.waiters.splice(this.waiters.indexOf(waiter), 1)
        reject(new Error(`等消息超时，已收到：${JSON.stringify(this.frames)}`))
      }, timeoutMs)
      this.waiters.push(waiter)
    })
  }

  close() {
    this.socket.close()
  }
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
const room = `e2e-${Math.random().toString(16).slice(2, 10)}`

/* ------------------------------------------------------------ 1. 两端进配对房间 */

const a = new Peer(`${room}::pair`, 'dsh-ui-aaaa1111')
await a.ready
const first = await a.next((m) => m.t === 'peer_status')
assert.equal(first.peer_online, false, '第一个进房间的应当看到「还没对端」')

const b = new Peer(`${room}::pair`, 'dsh-ui-bbbb2222')
await b.ready

const [aSees, bSees] = await Promise.all([
  a.next((m) => m.t === 'peer_status' && m.peer_online === true),
  b.next((m) => m.t === 'peer_status' && m.peer_online === true),
])
assert.equal(aSees.peer_device_id, 'dsh-ui-bbbb2222', 'A 看到的对端 id 不对')
assert.equal(bSees.peer_device_id, 'dsh-ui-aaaa1111', 'B 看到的对端 id 不对')
console.log('PASS 两端进配对房间后互相看到（后进的一方会触发双方各一条 peer_status）')

/* ------------------------------------------------------------ 2. 真房间不受影响 */

const daemon = new Peer(room, '11111111-2222-3333-4444-555555555555')
await daemon.ready
const daemonFirst = await daemon.next((m) => m.t === 'peer_status')
assert.equal(daemonFirst.peer_online, false, '真房间只有 daemon 时不该说「有对端」')
assert.equal(a.frames.filter((m) => m.t === 'peer_status' && m.peer_online === true).length, 1, '配对房间被真房间的连接干扰了')
console.log('PASS 真房间与配对房间互不干扰（daemon 不会被 UI 的等待连接当对端）')

/* ------------------------------------------------------------ 3. 同 device 重连是替换 */

const again = new Peer(`${room}::pair`, 'dsh-ui-aaaa1111')
await again.ready
const replaced = await a.next((m) => m.t === 'bye', 3000)
assert.match(replaced.reason, /同一设备/, '同 device 重连没有替换旧连接')
await sleep(150)
// 替换成功后中继会再推一次「配对成功」（旧端走了、新端在），对端仍只看到同一台设备
const lastB = b.frames.filter((m) => m.t === 'peer_status').pop()
assert.equal(lastB.peer_online, true, '重连后对端反而看不到人了')
assert.equal(lastB.peer_device_id, 'dsh-ui-aaaa1111', '重连后对端认到的设备变了')
assert.ok(!again.frames.some((m) => m.t === 'bye'), '新连接被当成第三个端挤掉了')
console.log('PASS 同 device_id 重连会替换自己，不会占掉第二个位')

/* ------------------------------------------------------------ 4. 离开会通知另一端 */

again.close()
await sleep(150)
const gone = await b.next((m) => m.t === 'peer_status' && m.peer_online === false, 3000)
assert.equal(gone.peer_online, false, '对端离开后没有收到 peer_online=false')
console.log('PASS 一端离房，另一端立刻收到 peer_online=false')

/* ------------------------------------------------------------ 清理 */

for (const peer of [a, b, daemon]) peer.close()
await sleep(50)
console.log('pairing-e2e: 全部通过')
process.exit(0)
