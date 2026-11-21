/**
 * 四个 agent 工具：让 dsh 里的模型能直接问「同步好了吗」，而不必让用户切界面。
 *
 * 约定（与 dsh 其它工具一致）：
 *   - 只返回 `{ok:true,data}` 或 `{ok:false,error:{code,message}}`，不抛异常
 *   - 只读工具绝不改状态；`winsync_sync_now` 有副作用，描述里写明
 *   - 永远不返回 token，也不返回 daemon 的端口/握手细节
 *
 * 这里不用 `defineTool`：它对 parameters 的 DSL 做了二次转换，而本插件是手写
 * ESM、不参与打包，直接给 `ctx.tools.register` 标准 JSON Schema 更稳。
 */

import { DaemonError } from './daemon.js'

const textRender = (_args, value) => [{ type: 'text', text: JSON.stringify(value, null, 2) }]

/** 统一输出声明：结构由 render 决定，schema 放宽以免字段增减就校验失败。 */
const output = {
  schema: { type: 'object', additionalProperties: true },
  render: textRender,
}

function ok(data) {
  return { ok: true, ...data }
}

function fail(error) {
  const code = error?.code ?? 'daemon-unavailable'
  return { ok: false, error: { code, message: error?.message ?? String(error) } }
}

function taskSummary(task) {
  return {
    id: task.id,
    name: task.display_name,
    root: task.root,
    server: task.server_short,
    workspace: task.workspace,
    mode: task.mode,
    paused: task.paused,
    running: task.runtime?.running ?? false,
    issues: task.issues ?? [],
    has_token: task.has_token,
  }
}

function runtimeOf(task) {
  const rt = task.runtime ?? {}
  return {
    running: rt.running ?? false,
    progress: rt.progress ?? null,
    last: rt.last ?? null,
    last_error: rt.last_error ?? null,
    last_time: rt.last_time ?? null,
    next_auto_in: rt.next_auto_in ?? null,
  }
}

/** 按 id 或名字找任务；不传就取第一个。返回 null 表示没找到。 */
function pickTask(state, wanted) {
  const tasks = state?.tasks ?? []
  if (tasks.length === 0) return null
  if (wanted === undefined || wanted === null || wanted === '') return tasks[0]
  const needle = String(wanted).toLowerCase()
  return (
    tasks.find((t) => t.id === wanted) ??
    tasks.find((t) => t.name.toLowerCase() === needle) ??
    tasks.find((t) => t.display_name?.toLowerCase() === needle) ??
    null
  )
}

async function getState(manager) {
  const reply = await manager.request('/api/state')
  return manager.constructor.unwrap(reply)
}

export function registerTools(ctx, manager) {
  const disposers = []

  disposers.push(
    ctx.tools.register({
      name: 'winsync_tasks',
      description:
        '列出 WinSync 同步任务（只读）：id、名称、本机目录、目标服务器/房间、方向、是否暂停、是否正在同步、配置问题。密钥只以 has_token 表示，绝不返回明文。',
      parameters: { type: 'object', properties: {}, additionalProperties: false },
      output,
      execute: async () => {
        try {
          const state = await getState(manager)
          return ok({
            data: {
              auto: state.auto,
              tasks: (state.tasks ?? []).map(taskSummary),
            },
          })
        } catch (error) {
          return fail(error)
        }
      },
    }),
  )

  disposers.push(
    ctx.tools.register({
      name: 'winsync_status',
      description:
        '查询某个任务（或全部任务）的同步状态（只读）：是否正在同步、进度、上次结果（推送/拉取/删除/冲突/字节数）、上次失败原因、下次自动同步倒计时。不传 task 时返回第一个任务；传 id 或名称。',
      parameters: {
        type: 'object',
        properties: {
          task: { type: 'string', description: '任务 id 或名称；省略则取第一个任务' },
        },
        additionalProperties: false,
      },
      output,
      execute: async (args) => {
        try {
          const state = await getState(manager)
          const task = pickTask(state, args?.task)
          if (task === null) {
            return fail(new DaemonError('not-found', '还没有配置任何同步任务'))
          }
          return ok({
            data: {
              task: taskSummary(task),
              runtime: runtimeOf(task),
              auto: state.auto,
              issues: task.issues ?? [],
            },
          })
        } catch (error) {
          return fail(error)
        }
      },
    }),
  )

  disposers.push(
    ctx.tools.register({
      name: 'winsync_sync_now',
      description:
        '立刻触发一次同步。注意这是**有副作用**的操作：双向模式下会把本机变更推给对端，也会把对端的删除传播到本机。调用前建议先用 winsync_status 确认该任务当前没有在同步。默认只返回「已开始」，wait=true 时等到结束并返回本次报告（最多 timeoutSec 秒）。',
      parameters: {
        type: 'object',
        properties: {
          task: { type: 'string', description: '任务 id 或名称；省略则取第一个任务' },
          wait: { type: 'boolean', description: '是否等同步结束再返回（默认 false）' },
          timeoutSec: { type: 'number', description: 'wait=true 时的最长等待秒数，最大 120，默认 60' },
        },
        additionalProperties: false,
      },
      output,
      execute: async (args, exec) => {
        try {
          const state = await getState(manager)
          const task = pickTask(state, args?.task)
          if (task === null) return fail(new DaemonError('not-found', '还没有配置任何同步任务'))
          if (task.issues?.length > 0) {
            return fail(new DaemonError('not-ready', task.issues.join('；')))
          }
          const reply = await manager.request(`/api/tasks/${encodeURIComponent(task.id)}/sync`, {
            method: 'POST',
            timeoutMs: 15_000,
          })
          manager.constructor.unwrap(reply)

          if (args?.wait !== true) {
            return ok({ data: { started: true, task: taskSummary(task) } })
          }

          const timeoutSec = Math.min(Math.max(Number(args?.timeoutSec) || 60, 1), 120)
          const deadline = Date.now() + timeoutSec * 1000
          while (Date.now() < deadline) {
            if (exec?.signal?.aborted) {
              return fail(new DaemonError('aborted', '调用已取消；同步仍在后台继续'))
            }
            await new Promise((resolve) => setTimeout(resolve, 500))
            const latest = await getState(manager)
            const current = pickTask(latest, task.id)
            if (current !== null && current.runtime?.running === false) {
              return ok({ data: { started: true, task: taskSummary(current), runtime: runtimeOf(current) } })
            }
          }
          return ok({
            data: {
              started: true,
              timedOut: true,
              task: taskSummary(task),
              hint: `等了 ${timeoutSec} 秒还没结束；同步仍在后台跑，可用 winsync_status 继续查`,
            },
          })
        } catch (error) {
          return fail(error)
        }
      },
    }),
  )

  disposers.push(
    ctx.tools.register({
      name: 'winsync_recent_activity',
      description:
        '查看最近的同步记录（只读）：每个文件的动作（上传/下载/删除/冲突副本/建目录）、成功与否、时间。用于回答「刚才同步了什么」「哪些文件失败了」。',
      parameters: {
        type: 'object',
        properties: {
          task: { type: 'string', description: '任务 id 或名称；省略则取第一个任务' },
          limit: { type: 'number', description: '最多返回多少条，最大 200，默认 30' },
          onlyFailed: { type: 'boolean', description: '只看失败的记录' },
        },
        additionalProperties: false,
      },
      output,
      execute: async (args) => {
        try {
          const state = await getState(manager)
          const task = pickTask(state, args?.task)
          if (task === null) return fail(new DaemonError('not-found', '还没有配置任何同步任务'))
          const limit = Math.min(Math.max(Number(args?.limit) || 30, 1), 200)
          const query = new URLSearchParams({
            task: task.id,
            limit: String(limit),
            status: args?.onlyFailed === true ? 'fail' : 'all',
          })
          const reply = await manager.request(`/api/activities?${query.toString()}`)
          const body = manager.constructor.unwrap(reply)
          return ok({
            data: {
              task: taskSummary(task),
              total: body.total ?? body.rows?.length ?? 0,
              rows: (body.rows ?? []).map((row) => ({
                time: row.created_at,
                file: row.path,
                action: row.action,
                ok: row.ok,
                message: row.message,
              })),
            },
          })
        } catch (error) {
          return fail(error)
        }
      },
    }),
  )

  return () => {
    for (const dispose of disposers) {
      try {
        dispose()
      } catch {
        /* 已经卸载过了 */
      }
    }
  }
}
