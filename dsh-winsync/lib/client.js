/**
 * dsh-winsync — browser half (P0 skeleton).
 *
 * Two registrations, both global (not session-bound):
 *   - `main` keyed slot, key `winsync` — the page that fills the centre column.
 *   - `sidebar.panellist`, id `winsync` — the icon in the left sidebar whose
 *     button selects that page. The sidebar owns the button, tooltip, label and
 *     selected state; this file only supplies the glyph.
 *
 * Inside the page: the three P0 self-checks. No WinSync UI yet — P3 replaces
 * `WinsyncPage`'s body with the sidebar + task/log/settings pages.
 *
 * Hand-authored bundle: the shell materializes it through
 * `window.__ModuleLoader__.load({ id, factory })`, where `factory(require)` returns
 * this package's client exports. Only baseline modules are requested (`react`), so
 * the package needs no `dsh.client.external` entries.
 */

window.__ModuleLoader__.load({
  id: 'dsh-winsync',
  factory: (require) => {
    const React = require('react')
    const h = React.createElement

    /** Shared by the sidebar entry and the main panel it opens. */
    const PANEL_ID = 'winsync'

    async function api(route, method) {
      const response = await fetch(`/winsync/api${route}`, {
        method: method ?? 'GET',
        headers: { accept: 'application/json' },
      })
      const text = await response.text()
      let body
      try {
        body = JSON.parse(text)
      } catch {
        body = { raw: text }
      }
      if (!response.ok) {
        const message = body?.error?.message ?? `HTTP ${response.status}`
        const error = new Error(message)
        error.body = body
        throw error
      }
      return body
    }

    const styles = {
      root: {
        boxSizing: 'border-box',
        height: '100%',
        overflow: 'auto',
        padding: '28px 32px 40px',
        font: '13px/1.55 ui-sans-serif, system-ui, "Segoe UI", sans-serif',
        color: 'inherit',
      },
      title: { margin: '0 0 4px', fontSize: 16, fontWeight: 600 },
      hint: { margin: '0 0 20px', opacity: 0.62 },
      card: {
        border: '1px solid color-mix(in srgb, currentColor 16%, transparent)',
        borderRadius: 10,
        padding: '14px 16px',
        marginBottom: 14,
        maxWidth: 760,
      },
      row: { display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap' },
      button: {
        appearance: 'none',
        border: '1px solid color-mix(in srgb, currentColor 22%, transparent)',
        background: 'color-mix(in srgb, currentColor 8%, transparent)',
        color: 'inherit',
        borderRadius: 7,
        padding: '5px 12px',
        font: 'inherit',
        cursor: 'pointer',
      },
      code: {
        font: '12px/1.5 ui-monospace, SFMono-Regular, Consolas, monospace',
        whiteSpace: 'pre-wrap',
        wordBreak: 'break-all',
        margin: '10px 0 0',
        opacity: 0.86,
      },
      pill: (tone) => ({
        display: 'inline-block',
        borderRadius: 999,
        padding: '1px 9px',
        fontSize: 12,
        border: '1px solid currentColor',
        color:
          tone === 'ok'
            ? '#3fb950'
            : tone === 'bad'
              ? '#f85149'
              : tone === 'busy'
                ? '#d29922'
                : 'inherit',
        opacity: tone === 'idle' ? 0.6 : 1,
      }),
    }

    function Card({ title, children }) {
      return h(
        'section',
        { style: styles.card },
        h('div', { style: { fontWeight: 600, marginBottom: 8 } }, title),
        children,
      )
    }

    /**
     * Sidebar glyph: two arrows chasing each other. Drawn at the edge the
     * sidebar asks for, in `currentColor` so it inherits the row's state colour.
     */
    function WinsyncIcon({ size }) {
      const edge = typeof size === 'number' && size > 0 ? size : 16
      return h(
        'svg',
        {
          width: edge,
          height: edge,
          viewBox: '0 0 16 16',
          fill: 'none',
          stroke: 'currentColor',
          strokeWidth: 1.5,
          strokeLinecap: 'round',
          strokeLinejoin: 'round',
          'aria-hidden': 'true',
        },
        h('path', { d: 'M2.8 6.6a5.1 5.1 0 0 1 9.5-1.9' }),
        h('path', { d: 'M13.2 2.4v2.7H10.5' }),
        h('path', { d: 'M13.2 9.4a5.1 5.1 0 0 1-9.5 1.9' }),
        h('path', { d: 'M2.8 13.6v-2.7h2.7' }),
      )
    }

    function WinsyncPage() {
      const [phase, setPhase] = React.useState('idle')
      const [message, setMessage] = React.useState('')
      const [payload, setPayload] = React.useState(null)

      const run = React.useCallback(async (label, route, method) => {
        setPhase('busy')
        setMessage(`${label}…`)
        setPayload(null)
        try {
          const body = await api(route, method)
          setPayload(body)
          setPhase('ok')
          setMessage(`${label} 成功`)
        } catch (error) {
          setPayload(error.body ?? null)
          setPhase('bad')
          setMessage(`${label} 失败：${error.message}`)
        }
      }, [])

      React.useEffect(() => {
        void run('探测插件路由', '/ping')
      }, [run])

      return h(
        'div',
        { style: styles.root },
        h('h2', { style: styles.title }, 'WinSync'),
        h('p', { style: styles.hint }, 'P0 骨架：只验证插件挂载、宿主路由与子进程握手，尚未接入同步功能。'),
        h(
          Card,
          { title: '1 · 插件与宿主路由' },
          h(
            'div',
            { style: styles.row },
            h(
              'span',
              { style: styles.pill(phase === 'bad' ? 'bad' : phase === 'busy' ? 'busy' : 'ok') },
              phase === 'bad' ? '失败' : phase === 'busy' ? '进行中' : '已连通',
            ),
            h('button', { style: styles.button, onClick: () => void run('探测插件路由', '/ping') }, '重新探测'),
            h('span', { style: { opacity: 0.7 } }, 'GET /winsync/api/ping'),
          ),
          message ? h('pre', { style: styles.code }, message) : null,
        ),
        h(
          Card,
          { title: '2 · 子进程握手（探针）' },
          h(
            'div',
            { style: styles.row },
            h('button', { style: styles.button, onClick: () => void run('启动探针', '/probe/start', 'POST') }, '启动'),
            h('button', { style: styles.button, onClick: () => void run('查询状态', '/probe/status') }, '状态'),
            h('button', { style: styles.button, onClick: () => void run('停止探针', '/probe/stop', 'POST') }, '停止'),
          ),
          message ? h('pre', { style: styles.code }, message) : null,
        ),
        payload ? h('pre', { style: styles.code }, JSON.stringify(payload, null, 2)) : null,
      )
    }

    return {
      name: 'dsh-winsync',
      inject: ['slots'],
      apply(ctx) {
        // Centre column page. The sidebar entry below selects it by the same id.
        ctx.slots.inject('main', () =>
          ctx.slots.register(
            {
              name: 'main',
              key: PANEL_ID,
            },
            WinsyncPage,
          ),
        )
        // Left sidebar icon list: one row per global panel, addressed by id.
        ctx.slots.inject('sidebar.panellist', () =>
          ctx.slots.register(
            {
              name: 'sidebar.panellist',
              id: PANEL_ID,
              order: 10,
              label: () => '同步',
            },
            WinsyncIcon,
          ),
        )
      },
    }
  },
})
