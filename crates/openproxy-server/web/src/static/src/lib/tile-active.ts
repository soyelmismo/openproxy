// Is a view's tile active, or backgrounded? While one tile is maximized every
// other tile is hidden behind it: their continuous cycles (clock ticks, WS
// repaints, status polls) skip work until the hero is restored. The workspace
// marks the state on body so a view can check its own tile with a DOM read
// instead of reaching into workspace state.
export function tileRootActive(root: ParentNode | null): boolean {
  if (!root || !("closest" in root)) return true;
  const tile = (root as Element).closest(".workspace-tile");
  if (!tile) return true;
  if ((tile as HTMLElement).hidden) return false;
  if (!document.body.classList.contains("workspace-maximized")) return true;
  return (tile as HTMLElement).dataset["panel"] === document.body.dataset["maximizedTile"];
}
