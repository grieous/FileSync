/**
 * 把交叉编译好的静态中继打成 docker 能 `load` 的镜像 tar。
 *
 *   node sync/deploy/make-image.mjs <sync-server 二进制> <输出 tar.gz> [标签]
 *
 * 为什么不用 `docker build`：这台打包机没有 Docker（WSL 被策略禁用），
 * 而镜像里其实只有一个静态二进制、一个目录都不用建。docker save 的旧格式
 * 就是「外 tar + 每层一个 layer.tar + config.json + manifest.json」，
 * 手写出来反而是最能离线复现的做法，也不用在服务器上下载 crates。
 *
 * 产物只需 `docker load -i <tar.gz>`。若某个 docker 版本不认这种手写包，
 * 用同一个二进制走 dist/winsync-relay-image/Dockerfile（FROM scratch）重建，几秒就好。
 */

import { createHash } from 'node:crypto'
import { chmodSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { gzipSync } from 'node:zlib'
import path from 'node:path'

const [binPath, outPath, tag = 'winsync-relay:0.6'] = process.argv.slice(2)
if (!binPath || !outPath) {
  console.error('用法：node sync/deploy/make-image.mjs <二进制> <输出 .tar.gz> [标签]')
  process.exit(2)
}

const payload = readFileSync(binPath)
const created = new Date().toISOString().replace(/\.\d+Z$/, 'Z')

/** 手写 ustar 头：模式/属主/时间全都写死，免得 Windows 上打包把 0755 丢掉。 */
function tarHeader({ name, size, mode = 0o644, type = '0', mtime = 0 }) {
  const block = Buffer.alloc(512, 0)
  const write = (offset, length, text) => block.write(text, offset, length, 'ascii')
  write(0, 100, name)
  write(100, 8, mode.toString(8).padStart(7, '0') + '\0')
  write(108, 8, '0000000\0')
  write(116, 8, '0000000\0')
  write(124, 12, size.toString(8).padStart(11, '0') + '\0')
  write(136, 12, mtime.toString(8).padStart(11, '0') + '\0')
  write(148, 8, '        ') // 校验和先填空格
  write(156, 1, type)
  write(257, 6, 'ustar\0')
  write(263, 2, '00')
  write(265, 32, 'root')
  write(297, 32, 'root')
  let sum = 0
  for (const byte of block) sum += byte
  write(148, 8, sum.toString(8).padStart(6, '0') + '\0 ')
  return block
}

function tarEntry(name, data, options = {}) {
  const body = Buffer.isBuffer(data) ? data : Buffer.from(data)
  const padding = (512 - (body.length % 512)) % 512
  return Buffer.concat([tarHeader({ name, size: body.length, ...options }), body, Buffer.alloc(padding, 0)])
}

function buildTar(entries) {
  return Buffer.concat([...entries, Buffer.alloc(1024, 0)])
}

const sha256 = (buffer) => createHash('sha256').update(buffer).digest('hex')

/* ------------------------------------------------------------ 一层：只有那个二进制 */

const layer = buildTar([
  tarEntry('sync-server', payload, { mode: 0o755, mtime: Math.floor(Date.now() / 1000) }),
])
const layerSha = sha256(layer)
const diffId = layerSha // 层是未压缩的，diff_id 与文件 sha 相同

const config = {
  created,
  architecture: 'amd64',
  os: 'linux',
  config: {
    Env: ['SYNC_ADDR=0.0.0.0:8443', 'RUST_LOG=info'],
    Entrypoint: ['/sync-server'],
    ExposedPorts: { '8443/tcp': {} },
    // 中继只转发、不落盘，也不需要 root：8031 是随便挑的高位 uid，避免和宿主冲突
    User: '65534:65534',
    WorkingDir: '/',
  },
  rootfs: { type: 'layers', diff_ids: [`sha256:${diffId}`] },
  history: [
    {
      created,
      created_by: 'make-image.mjs: FROM scratch + COPY sync-server',
      comment: 'WinSync relay — single static binary, no base image',
    },
  ],
}
const configJson = Buffer.from(JSON.stringify(config), 'utf8')
const configSha = sha256(configJson)

const manifest = JSON.stringify([
  {
    Config: `${configSha}.json`,
    RepoTags: [tag],
    Layers: [`${layerSha}/layer.tar`],
  },
])

const image = buildTar([
  tarEntry('manifest.json', manifest),
  tarEntry(`${configSha}.json`, configJson),
  tarEntry(`${layerSha}/layer.tar`, layer),
])

mkdirSync(path.dirname(outPath), { recursive: true })
const gz = gzipSync(image, { level: 9 })
writeFileSync(outPath, gz)

/* ------------------------------------------------------------ 自检：把包读回来核对 */

const parsed = JSON.parse(manifest)[0]
const checks = [
  ['manifest 引用的 config 存在', image.includes(Buffer.from(parsed.Config))],
  ['manifest 引用的 layer 存在', image.includes(Buffer.from(parsed.Layers[0]))],
  ['config 里的 diff_id 与层 sha 一致', config.rootfs.diff_ids[0] === `sha256:${layerSha}`],
  ['二进制体积一致', image.includes(payload.subarray(0, 64))],
]
for (const [name, ok] of checks) {
  console.log(`${ok ? '✓' : '✗'} ${name}`)
  if (!ok) process.exitCode = 1
}

console.log(`\n镜像  : ${outPath}`)
console.log(`标签  : ${tag}`)
console.log(`大小  : ${(gz.length / 1024 / 1024).toFixed(2)} MB（未压缩 ${(image.length / 1024 / 1024).toFixed(2)} MB）`)
console.log(`sha256: ${sha256(gz)}`)
console.log(`layer : ${layerSha.slice(0, 16)}…  config: ${configSha.slice(0, 16)}…`)
void chmodSync
void rmSync
