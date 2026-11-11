import { describe, expect, it } from 'vitest';
import { computeGridTracks } from './data-grid-layout.js';

describe('data grid usable-width layout', () => {
  it('shrinks the preferred name floor instead of pushing actions outside an ultra-compact tile', () => {
    const result = computeGridTracks([360, 70, 88], [180, 60], [0, 1], 0, 210, 2);
    expect(result.template).toBe('minmax(122px, 1fr) 0px 88px');
    expect(result.active).toEqual([true, false, true]);
  });

  it('keeps columns at desktop widths and folds in priority order', () => {
    expect(computeGridTracks([360, 70, 88], [180, 60], [0, 1], 0, 800, 2))
      .toEqual({ template: 'minmax(180px, 1fr) 70px 88px', active: [true, true, true] });
    expect(computeGridTracks([300, 60, 90, 88], [160], [0, 2, 1], 0, 320, 3).active)
      .toEqual([true, true, false, true]);
  });

  it('never allocates more than the available width after folding at any compact size', () => {
    for (let width = 0; width <= 800; width++) {
      const { template } = computeGridTracks([360.4, 70.3, 100.8, 88], [180], [0, 2, 1], 0, width, 3);
      const tracks = [...template.matchAll(/(?:minmax\()?(\d+)px/g)].map(m => Number(m[1]));
      expect(tracks.reduce((sum, value) => sum + value, 0)).toBeLessThanOrEqual(width);
    }
  });

  it('folds pinned metadata as a last resort so identity remains readable', () => {
    expect(computeGridTracks([360, 62, 102], [160], [0, 0], 0, 198, 2))
      .toEqual({ template: 'minmax(96px, 1fr) 0px 102px', active: [true, false, true] });
  });

  it('fits grids without an elastic column or foldable columns', () => {
    expect(computeGridTracks([160, 180], [], [], -1, 200).template).toBe('94px 105px');
  });
});
