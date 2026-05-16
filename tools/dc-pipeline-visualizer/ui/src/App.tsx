import { useEffect, useMemo, useState } from 'react'

type MetricSelector = {
  metric_exact?: string
  metric_prefix?: string
  metric_type?: string
  process?: string
  run?: string
  workload?: string
  variant?: string
}

type GraphNode = {
  id: string
  label: string
  kind?: string
  selectors?: MetricSelector[]
  x?: number
  y?: number
}

type GraphEdge = {
  id: string
  from: string
  to: string
  label?: string
  selectors?: MetricSelector[]
}

type GraphConfig = {
  nodes: GraphNode[]
  edges: GraphEdge[]
}

type MetricRow = {
  metric: string
  type: string
  variant?: string
  process?: string
  run?: string
  workloads?: string[]
  ts?: number
  enq?: number
  drain?: number
  depth?: number
  count?: number
  p99?: number
  p50?: number
  max?: number
  unit?: string
}

type Alert = {
  kind: string
  severity: 'warning' | 'critical' | string
  message: string
}

type TimePoint = {
  ts: number
  depth?: number
  enq?: number
  drain?: number
  p99?: number
}

type MetricSeries = {
  key: string
  latest: MetricRow
  history: TimePoint[]
  alerts: Alert[]
}

type Snapshot = {
  metrics: MetricSeries[]
  available_runs: string[]
  available_processes: string[]
  available_workloads: string[]
}

type BootstrapMessage = {
  type: 'bootstrap'
  graph: GraphConfig
  snapshot: Snapshot
  now_ts: number
}

type MetricsMessage = {
  type: 'metrics'
  updates: MetricSeries[]
}

type HealthMessage = {
  type: 'health'
  status: string
  message: string
}

type StateMessage = {
  type: 'state'
  state: string
  detail: string
}

type ServerMessage = BootstrapMessage | MetricsMessage | HealthMessage | StateMessage

type Filters = {
  run: string
  process: string
  workload: string
}

const defaultGraph: GraphConfig = { nodes: [], edges: [] }

function wsUrl() {
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${protocol}//${window.location.host}/ws`
}

function selectorMatches(selector: MetricSelector, row: MetricRow) {
  if (selector.metric_exact && row.metric !== selector.metric_exact) return false
  if (selector.metric_prefix && !row.metric.startsWith(selector.metric_prefix)) return false
  if (selector.metric_type && row.type !== selector.metric_type) return false
  if (selector.process && row.process !== selector.process) return false
  if (selector.run && row.run !== selector.run) return false
  if (selector.workload && !(row.workloads ?? []).includes(selector.workload)) return false
  if (selector.variant && row.variant !== selector.variant) return false
  return true
}

function metricMatchesSelectors(row: MetricRow, selectors: MetricSelector[] | undefined) {
  if (!selectors || selectors.length === 0) return true
  return selectors.some((selector) => selectorMatches(selector, row))
}

function sparklinePoints(history: TimePoint[], field: 'depth' | 'p99') {
  const values = history
    .map((point) => (field === 'depth' ? point.depth : point.p99))
    .filter((value): value is number => value !== undefined)

  if (values.length < 2) return ''

  const min = Math.min(...values)
  const max = Math.max(...values)
  const span = Math.max(max - min, 1)

  return values
    .map((value, index) => {
      const x = (index / (values.length - 1)) * 100
      const y = 100 - ((value - min) / span) * 100
      return `${x},${y}`
    })
    .join(' ')
}

function highestSeverity(alerts: Alert[]) {
  if (alerts.some((alert) => alert.severity === 'critical')) return 'critical'
  if (alerts.some((alert) => alert.severity === 'warning')) return 'warning'
  return 'ok'
}

function App() {
  const [graph, setGraph] = useState<GraphConfig>(defaultGraph)
  const [seriesMap, setSeriesMap] = useState<Record<string, MetricSeries>>({})
  const [filters, setFilters] = useState<Filters>({ run: 'all', process: 'all', workload: 'all' })
  const [runs, setRuns] = useState<string[]>([])
  const [processes, setProcesses] = useState<string[]>([])
  const [workloads, setWorkloads] = useState<string[]>([])
  const [selectedNode, setSelectedNode] = useState<string | null>(null)
  const [connectionStatus, setConnectionStatus] = useState('connecting')
  const [healthMessage, setHealthMessage] = useState('')

  useEffect(() => {
    const socket = new WebSocket(wsUrl())

    socket.onopen = () => {
      setConnectionStatus('connected')
      setHealthMessage('')
    }

    socket.onmessage = (event) => {
      const message = JSON.parse(event.data) as ServerMessage
      if (message.type === 'bootstrap') {
        setGraph(message.graph)
        const next: Record<string, MetricSeries> = {}
        for (const series of message.snapshot.metrics) {
          next[series.key] = series
        }
        setSeriesMap(next)
        setRuns(message.snapshot.available_runs)
        setProcesses(message.snapshot.available_processes)
        setWorkloads(message.snapshot.available_workloads)
      } else if (message.type === 'metrics') {
        setSeriesMap((current) => {
          const next = { ...current }
          for (const update of message.updates) {
            next[update.key] = update
          }
          return next
        })
      } else if (message.type === 'health') {
        setHealthMessage(`${message.status}: ${message.message}`)
      } else if (message.type === 'state') {
        setHealthMessage(`${message.state}: ${message.detail}`)
      }
    }

    socket.onclose = () => {
      setConnectionStatus('disconnected')
      setHealthMessage('websocket disconnected')
    }

    socket.onerror = () => {
      setConnectionStatus('error')
      setHealthMessage('websocket error')
    }

    return () => socket.close()
  }, [])

  const allSeries = useMemo(() => Object.values(seriesMap), [seriesMap])

  const filteredSeries = useMemo(
    () =>
      allSeries.filter((series) => {
        const row = series.latest
        if (filters.run !== 'all' && row.run !== filters.run) return false
        if (filters.process !== 'all' && row.process !== filters.process) return false
        if (filters.workload !== 'all' && !(row.workloads ?? []).includes(filters.workload)) return false
        return true
      }),
    [allSeries, filters],
  )

  const nodeMetrics = useMemo(() => {
    const map: Record<string, MetricSeries[]> = {}
    for (const node of graph.nodes) {
      map[node.id] = filteredSeries.filter((series) => metricMatchesSelectors(series.latest, node.selectors))
    }
    return map
  }, [graph.nodes, filteredSeries])

  const edgeMetrics = useMemo(() => {
    const map: Record<string, MetricSeries[]> = {}
    for (const edge of graph.edges) {
      map[edge.id] = filteredSeries.filter((series) => metricMatchesSelectors(series.latest, edge.selectors))
    }
    return map
  }, [graph.edges, filteredSeries])

  const selectedMetrics = selectedNode ? nodeMetrics[selectedNode] ?? [] : []

  return (
    <div className="min-h-screen bg-slate-950 text-slate-100">
      <header className="border-b border-slate-800 bg-slate-900/70 px-6 py-4">
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div>
            <h1 className="text-xl font-semibold">s2n-quic-dc pipeline visualizer</h1>
            <p className="text-sm text-slate-300">Status: {connectionStatus}{healthMessage ? ` • ${healthMessage}` : ''}</p>
          </div>
          <div className="flex flex-wrap items-center gap-3 text-sm">
            <select className="rounded border border-slate-700 bg-slate-900 px-2 py-1" value={filters.run} onChange={(event) => setFilters((current) => ({ ...current, run: event.target.value }))}>
              <option value="all">Run: all</option>
              {runs.map((run) => (
                <option key={run} value={run}>{run}</option>
              ))}
            </select>
            <select className="rounded border border-slate-700 bg-slate-900 px-2 py-1" value={filters.process} onChange={(event) => setFilters((current) => ({ ...current, process: event.target.value }))}>
              <option value="all">Process: all</option>
              {processes.map((process) => (
                <option key={process} value={process}>{process}</option>
              ))}
            </select>
            <select className="rounded border border-slate-700 bg-slate-900 px-2 py-1" value={filters.workload} onChange={(event) => setFilters((current) => ({ ...current, workload: event.target.value }))}>
              <option value="all">Workload: all</option>
              {workloads.map((workload) => (
                <option key={workload} value={workload}>{workload}</option>
              ))}
            </select>
          </div>
        </div>
      </header>

      <main className="grid gap-6 p-6 lg:grid-cols-[2fr_1fr]">
        <section className="rounded-lg border border-slate-800 bg-slate-900 p-4">
          <h2 className="mb-3 text-lg font-medium">Pipeline graph</h2>
          <div className="relative h-[520px] overflow-auto rounded border border-slate-800 bg-slate-950">
            <svg className="absolute inset-0 h-full w-full" viewBox="0 0 1000 540" preserveAspectRatio="none">
              {graph.edges.map((edge) => {
                const from = graph.nodes.find((node) => node.id === edge.from)
                const to = graph.nodes.find((node) => node.id === edge.to)
                if (!from || !to) return null
                const severity = highestSeverity((edgeMetrics[edge.id] ?? []).flatMap((series) => series.alerts))
                const stroke = severity === 'critical' ? '#ef4444' : severity === 'warning' ? '#f59e0b' : '#334155'
                return (
                  <g key={edge.id}>
                    <line x1={(from.x ?? 120) + 120} y1={(from.y ?? 80) + 32} x2={to.x ?? 360} y2={(to.y ?? 80) + 32} stroke={stroke} strokeWidth={3} markerEnd="url(#arrow)" />
                    <text x={((from.x ?? 120) + (to.x ?? 360)) / 2 + 20} y={((from.y ?? 80) + (to.y ?? 80)) / 2 + 24} fill="#cbd5e1" fontSize="12">{edge.label ?? edge.id}</text>
                  </g>
                )
              })}
              <defs>
                <marker id="arrow" markerWidth="10" markerHeight="8" refX="9" refY="4" orient="auto">
                  <polygon points="0 0, 10 4, 0 8" fill="#64748b" />
                </marker>
              </defs>
            </svg>

            {graph.nodes.map((node) => {
              const metrics = nodeMetrics[node.id] ?? []
              const queue = metrics.find((series) => series.latest.type === 'queue')?.latest
              const task = metrics.find((series) => series.latest.type === 'histogram' && series.latest.metric.startsWith('task'))?.latest
              const severity = highestSeverity(metrics.flatMap((series) => series.alerts))
              return (
                <button
                  key={node.id}
                  type="button"
                  onClick={() => setSelectedNode(node.id)}
                  className={`absolute w-56 rounded border p-3 text-left transition ${selectedNode === node.id ? 'ring-2 ring-sky-500' : ''} ${severity === 'critical' ? 'border-red-500 bg-red-950/40' : severity === 'warning' ? 'border-amber-500 bg-amber-950/30' : 'border-slate-700 bg-slate-900'}`}
                  style={{ left: node.x ?? 80, top: node.y ?? 80 }}
                >
                  <div className="text-sm font-semibold">{node.label}</div>
                  <div className="text-xs text-slate-400">{node.kind ?? 'stage'}</div>
                  {queue && (
                    <div className="mt-2 text-xs text-slate-200">
                      q.depth={queue.depth ?? 0} • enq={queue.enq ?? 0} • drain={queue.drain ?? 0}
                    </div>
                  )}
                  {task && (
                    <div className="mt-1 text-xs text-slate-200">
                      task.p99={task.p99?.toFixed(3) ?? '0'} {task.unit ?? ''}
                    </div>
                  )}
                </button>
              )
            })}
          </div>
        </section>

        <section className="rounded-lg border border-slate-800 bg-slate-900 p-4">
          <h2 className="mb-3 text-lg font-medium">Node drill-down</h2>
          {!selectedNode ? (
            <p className="text-sm text-slate-300">Select a node to inspect queue/task history and alerts.</p>
          ) : (
            <div className="space-y-4">
              <div>
                <h3 className="text-sm font-semibold">{graph.nodes.find((node) => node.id === selectedNode)?.label ?? selectedNode}</h3>
                <p className="text-xs text-slate-400">{selectedMetrics.length} matching metric series</p>
              </div>
              {selectedMetrics.map((series) => {
                const queueSparkline = sparklinePoints(series.history, 'depth')
                const taskSparkline = sparklinePoints(series.history, 'p99')
                return (
                  <article key={series.key} className="rounded border border-slate-700 p-3">
                    <div className="text-xs text-slate-300">{series.latest.metric}{series.latest.variant ? `:${series.latest.variant}` : ''}</div>
                    <div className="text-xs text-slate-500">{series.latest.type} • {series.latest.process ?? 'n/a'}</div>
                    {queueSparkline && (
                      <svg className="mt-2 h-16 w-full rounded bg-slate-950" viewBox="0 0 100 100" preserveAspectRatio="none">
                        <polyline points={queueSparkline} fill="none" stroke="#60a5fa" strokeWidth="2" />
                      </svg>
                    )}
                    {!queueSparkline && taskSparkline && (
                      <svg className="mt-2 h-16 w-full rounded bg-slate-950" viewBox="0 0 100 100" preserveAspectRatio="none">
                        <polyline points={taskSparkline} fill="none" stroke="#a78bfa" strokeWidth="2" />
                      </svg>
                    )}
                    {series.alerts.length > 0 && (
                      <ul className="mt-2 space-y-1 text-xs">
                        {series.alerts.map((alert, idx) => (
                          <li key={`${series.key}-${idx}`} className={alert.severity === 'critical' ? 'text-red-300' : 'text-amber-300'}>
                            {alert.kind}: {alert.message}
                          </li>
                        ))}
                      </ul>
                    )}
                  </article>
                )
              })}
            </div>
          )}
        </section>
      </main>
    </div>
  )
}

export default App
