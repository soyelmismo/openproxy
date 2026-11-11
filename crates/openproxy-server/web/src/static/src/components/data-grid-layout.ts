// Pure sizing arithmetic. The caller supplies the row's usable content width,
// excluding borders and padding, not the outer grid width.
export function computeGridTracks(
  natural: readonly number[],
  mins: readonly number[],
  folds: readonly number[],
  elastic: number,
  available: number,
  actions: number = -1,
): { template: string; active: boolean[] } {
  const widths = natural.map((w) => Math.ceil(w));
  const floor = widths.map((_, i) => Math.max(mins[i] ?? 0, 24));
  const active = widths.map(() => true);
  const needed = (): number => widths.reduce((sum, w, i) =>
    sum + (active[i] ? i === elastic ? floor[i]! : w : 0), 0);

  while (needed() > available) {
    let victim = -1;
    let rank = Infinity;
    for (let i = 0; i < widths.length; i++) {
      const r = folds[i] ?? 0;
      if (active[i] && i !== elastic && i !== actions && r > 0 && r < rank) {
        rank = r;
        victim = i;
      }
    }
    if (victim === -1) break;
    active[victim] = false;
  }

  // At widths where even pinned metadata crowds out the primary label,
  // keep the identity + actions and expose metadata through row details.
  for (let i = widths.length - 1; elastic >= 0 && needed() > available && i >= 0; i--) {
    if (i !== elastic && i !== actions) active[i] = false;
  }

  // A column's preferred minimum is not permission to overflow its tile.
  // Preserve direct actions, then give the remaining columns only the space
  // that really exists. Folded content remains available in row details.
  const actionWidth = actions >= 0 ? Math.min(widths[actions] ?? 0, available) : 0;
  const fixed = widths.reduce((sum, w, i) =>
    sum + (active[i] && i !== elastic && i !== actions ? w : 0), 0);
  const budget = Math.max(0, available - actionWidth);
  const scale = fixed > budget ? budget / fixed : 1;
  const fixedWidth = widths.reduce((sum, w, i) =>
    sum + (active[i] && i !== elastic && i !== actions ? Math.floor(w * scale) : 0), 0);
  const elasticMin = Math.max(0, Math.min(floor[elastic] ?? 0, budget - fixedWidth));
  const template = widths.map((w, i) => {
    if (!active[i]) return '0px';
    if (i === elastic) return `minmax(${elasticMin}px, 1fr)`;
    if (i === actions) return `${actionWidth}px`;
    return `${Math.floor(w * scale)}px`;
  }).join(' ');
  return { template, active };
}
