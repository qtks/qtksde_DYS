export interface LayoutRect {
  x: number
  y: number
  w: number
  h: number
}

/**
 * Squarified treemap layout (Bruls/Huizing/van Wijk). Values are scaled to the
 * available area; the row with the lowest worst aspect ratio is placed first.
 */
export function squarify(values: number[], x: number, y: number, w: number, h: number): LayoutRect[] {
  const result: LayoutRect[] = []
  const total = values.reduce((sum, value) => sum + value, 0)
  if (total <= 0 || w <= 0 || h <= 0) {
    return values.map(() => ({ x, y, w: 0, h: 0 }))
  }
  const areas = values.map((value) => (value * w * h) / total)
  let rx = x
  let ry = y
  let rw = w
  let rh = h
  let row: number[] = []

  const worst = (candidate: number[], side: number): number => {
    const sum = candidate.reduce((acc, value) => acc + value, 0)
    if (sum <= 0) return Number.POSITIVE_INFINITY
    const max = Math.max(...candidate)
    const min = Math.min(...candidate)
    if (min <= 0) return Number.POSITIVE_INFINITY
    return Math.max((side * side * max) / (sum * sum), (sum * sum) / (side * side * min))
  }

  const placeRow = () => {
    const sum = row.reduce((acc, value) => acc + value, 0)
    if (sum <= 0) {
      row = []
      return
    }
    if (rw >= rh) {
      const strip = sum / rh
      let offset = ry
      for (const area of row) {
        const height = area / strip
        result.push({ x: rx, y: offset, w: strip, h: height })
        offset += height
      }
      rx += strip
      rw -= strip
    } else {
      const strip = sum / rw
      let offset = rx
      for (const area of row) {
        const width = area / strip
        result.push({ x: offset, y: ry, w: width, h: strip })
        offset += width
      }
      ry += strip
      rh -= strip
    }
    row = []
  }

  while (areas.length > 0) {
    const side = Math.min(rw, rh)
    const next = areas[0]
    if (row.length > 0 && worst([...row, next], side) > worst(row, side)) {
      placeRow()
      continue
    }
    row.push(areas.shift() as number)
  }
  if (row.length > 0) placeRow()
  return result
}
