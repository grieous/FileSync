/**
 * 打包插件：`npm pack` + 把 tgz 挪到输出目录，并强制要求包里带 daemon。
 *
 *   node scripts/pack.mjs [输出目录]
 *
 * 两个刻意的约束：
 *
 * 1. **没有 daemon 就直接失败**——「装了插件却提示 daemon-missing」比打包失败更难查。
 * 2. **打进去的是 `bin\winsync-daemon.exe.gz`，不是裸 exe**。pnpm 覆盖安装会整目录删掉
 *    `node_modules\dsh-winsync` 再重链，裸 exe 一旦被正在运行的 daemon 占住就删不掉：
 *    `[ERR_PNPM_EPERM] importPackage ... dsh-winsync_tmp_<pid>_<n>\bin`。
 *    压缩包是数据文件，不会被占用，也不会被杀软当可执行文件拦；插件首次启动时用
 *    `daemon.js#seedBinary()` 把它展开到 `%LOCALAPPDATA%\dsh-winsync\bin` 再运行。
 *
 * 仓库里的 `bin/winsync-daemon.exe` 是种子的来源（不进版本库），打包时现场压。
 */

import { createHash } from 'node:crypto'
import { execFileSync } from 'node:child_process'
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { gzipSync, gunzipSync } from 'node:zlib'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const pkgDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const pkg = JSON.parse(readFileSync(path.join(pkgDir, 'package.json'), 'utf8'))
const exe = path.join(pkgDir, 'bin', 'winsync-daemon.exe')
const outDir = path.resolve(process.argv[2] ?? path.join(pkgDir, '..', 'dist'))
const staging = path.join(pkgDir, '.pack-staging')

const fail = (lines) => {
  console.error(lines.join('\n'))
  process.exit(1)
}

if (!existsSync(exe)) {
  fail([
    `缺少 ${path.relative(pkgDir, exe)}`,
    '',
    '先把 daemon 编出来并放到这个位置：',
    '  cargo build --release -p sync-daemon --target x86_64-pc-windows-msvc',
    '  copy sync\\target\\release\\sync-daemon.exe dsh-winsync\\bin\\winsync-daemon.exe',
  ])
}

const exeSize = statSync(exe).size
if (exeSize < 64 * 1024) {
  fail([`${path.relative(pkgDir, exe)} 只有 ${exeSize} 字节，不像是真的可执行文件`])
}

const exeBuffer = readFileSync(exe)
const exeDigest = createHash('sha256').update(exeBuffer).digest('hex')

// ---------------------------------------------------------------- 组装暂存目录
rmSync(staging, { recursive: true, force: true })
mkdirSync(staging, { recursive: true })

const expected = new Set()
// npm 只会从 cwd 读 package.json、顺手带上 README；它们不在 files 白名单里，得自己拷
for (const always of ['README.md']) {
  const from = path.join(pkgDir, always)
  if (!existsSync(from)) continue
  copyFileSync(from, path.join(staging, always))
}

/**
 * 打包时把打包时间写进 package.json。
 *
 * 装到用户机器上时是没有 `.git` 的，也不用有：面板只报版本号与打包时间。
 * 提交号、分支、是否有未提交改动是打包机上的事，摆进界面只是噪音（见 lib/version.js）。
 */
const builtAt = new Date().toISOString()

writeFileSync(
  path.join(staging, 'package.json'),
  `${JSON.stringify({ ...pkg, builtAt }, null, 2)}\n`,
)

for (const rel of pkg.files) {
  // bin/ 下的东西由下面统一产出（只放压缩包），原样复制会把裸 exe 又带回去
  if (rel.startsWith('bin/')) continue
  const from = path.join(pkgDir, rel)
  if (!existsSync(from)) fail([`package.json 的 files 里列了不存在的文件：${rel}`])
  const to = path.join(staging, rel)
  mkdirSync(path.dirname(to), { recursive: true })
  copyFileSync(from, to)
  expected.add(rel.split(path.sep).join('/'))
}

const seed = gzipSync(exeBuffer, { level: 9 })
const seedRel = 'bin/winsync-daemon.exe.gz'
const seedPath = path.join(staging, 'bin', 'winsync-daemon.exe.gz')
mkdirSync(path.dirname(seedPath), { recursive: true })
writeFileSync(seedPath, seed)
expected.add(seedRel)

// 同时在仓库 bin/ 里留一份，方便开发时 `node test/seed.mjs` 与本地调试直接跑
writeFileSync(path.join(pkgDir, 'bin', 'winsync-daemon.exe.gz'), seed)

// 解压一遍，确认种子真的能还原成同一个 exe（打包错了当场发现，而不是用户机器上）
const restored = gunzipSync(readFileSync(seedPath))
if (createHash('sha256').update(restored).digest('hex') !== exeDigest) {
  fail(['种子回读校验失败：解压结果与源 exe 的 sha256 不一致'])
}

// ---------------------------------------------------------------- npm pack
// Windows 上 `npm` 是 npm.cmd，execFileSync 既不按 PATHEXT 补后缀、也不走 shell，
// 直接写 'npm' 会 ENOENT（node 自己装在 portable 目录里时更明显）。
// 用「当前 node + npm-cli.js」最稳，跟 PATH 里有没有 npm 无关。
const npmCli = path.join(path.dirname(process.execPath), 'node_modules', 'npm', 'bin', 'npm-cli.js')
const rawPack = existsSync(npmCli)
  ? execFileSync(process.execPath, [npmCli, 'pack', '--silent'], { cwd: staging, encoding: 'utf8' })
  : execFileSync('npm', ['pack', '--silent'], { cwd: staging, encoding: 'utf8', shell: process.platform === 'win32' })
// npm 输出以换行结尾，行尾还可能带 CR；不 trim 就会 rename 到一个带 \n 的名字
const packed = rawPack
  .split(/\r?\n/)
  .map((line) => line.trim())
  .filter(Boolean)
  .pop()
const tgz = path.join(staging, packed)

// 包里该有的都得有：pnpm 只认 files 白名单，漏一项就是装完少一个模块
// 用相对文件名 + cwd：Git 自带的 GNU tar 会把 `C:////...` 当成远程主机名，
// 报 "Cannot connect to C: resolve failed"。切到目录里再解就不带盘符了。
const listing = execFileSync('tar', ['-tzf', path.basename(tgz)], {
  cwd: path.dirname(tgz),
  encoding: 'utf8',
})
  .split('\n')
  .filter(Boolean)
  .map((line) => line.replace(/^package\//, ''))
  .filter((rel) => rel !== '' && !rel.endsWith('/'))
const missing = [...expected].filter((rel) => !listing.includes(rel))
if (missing.length > 0) fail([`打出来的包缺文件：${missing.join(', ')}`])
const strayExe = listing.find((rel) => rel.toLowerCase().endsWith('.exe'))
if (strayExe) fail([`包里出现了裸 exe（${strayExe}），覆盖安装会 EPERM，必须只留 .gz`])

mkdirSync(outDir, { recursive: true })
const target = path.join(outDir, packed)
renameSync(tgz, target)
rmSync(staging, { recursive: true, force: true })

const buffer = readFileSync(target)
console.log(path.relative(process.cwd(), target) || target)
console.log(`  version: ${pkg.version}（打包 ${builtAt}）`)
console.log(`  files:   ${listing.length}（含 ${seedRel}）`)
console.log(`  size:    ${(buffer.length / 1024 / 1024).toFixed(2)} MB`)
console.log(`  exe:     ${(exeSize / 1024 / 1024).toFixed(2)} MB → gz ${(seed.length / 1024 / 1024).toFixed(2)} MB`)
console.log(`  exe sha256: ${exeDigest}`)
console.log(`  tgz sha256: ${createHash('sha256').update(buffer).digest('hex')}`)
