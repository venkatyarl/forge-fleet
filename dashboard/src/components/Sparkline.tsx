// Zero-dep inline-SVG sparkline. Keeps the bundle small — no chart lib.
//
// Normalizes Y to the min/max of the supplied series so short idle
// stretches don't flatten out against an absolute axis. If you want an
// absolute axis (e.g. 0..100 for CPU%), pass `yMin`/`yMax`.

export function Sparkline({
  data,
  width = 100,
  height = 20,
  stroke = 'rgb(167 139 250)', // violet-400
  fill = 'rgba(167, 139, 250, 0.12)',
  yMin,
  yMax,
  title,
}: {
  data: Array<number | null | undefined>
  width?: number
  height?: number
  stroke?: string
  fill?: string
  yMin?: number
  yMax?: number
  title?: string
}) {
  // Drop null/undefined but preserve index spacing.
  const values = data.map((v) => (typeof v === 'number' && Number.isFinite(v) ? v : null))
  const nonNull = values.filter((v): v is number => v !== null)

  if (nonNull.length < 2) {
    return (
      <svg
        width={width}
        height={height}
        viewBox={`0 0 ${width} ${height}`}
        className="block"
        aria-label={title ?? 'sparkline'}
      >
        <line
          x1={0}
          y1={height / 2}
          x2={width}
          y2={height / 2}
          stroke="rgb(82 82 91)" // zinc-600
          strokeWidth={1}
          strokeDasharray="2 3"
        />
      </svg>
    )
  }

  const min = yMin ?? Math.min(...nonNull)
  const max = yMax ?? Math.max(...nonNull)
  const span = max - min || 1
  const stepX = values.length > 1 ? width / (values.length - 1) : width

  // Build a polyline — break the line wherever we hit a null so gaps
  // are visible rather than lying with a straight interpolation.
  const segments: string[] = []
  let open = false
  values.forEach((v, i) => {
    if (v === null) {
      open = false
      return
    }
    const x = i * stepX
    const y = height - ((v - min) / span) * height
    segments.push(`${open ? 'L' : 'M'}${x.toFixed(1)},${y.toFixed(1)}`)
    open = true
  })
  const linePath = segments.join(' ')

  // Area under the line (only for the contiguous span — good enough).
  const firstIdx = values.findIndex((v) => v !== null)
  const lastIdx = (() => {
    for (let i = values.length - 1; i >= 0; i--) if (values[i] !== null) return i
    return -1
  })()
  const areaPath =
    firstIdx >= 0 && lastIdx > firstIdx
      ? `${linePath} L${(lastIdx * stepX).toFixed(1)},${height} L${(firstIdx * stepX).toFixed(1)},${height} Z`
      : ''

  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      className="block"
      aria-label={title ?? 'sparkline'}
    >
      {areaPath && <path d={areaPath} fill={fill} stroke="none" />}
      <path d={linePath} fill="none" stroke={stroke} strokeWidth={1.25} strokeLinejoin="round" strokeLinecap="round" />
    </svg>
  )
}
