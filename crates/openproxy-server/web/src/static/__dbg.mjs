
import { createWorkspace, changeLayout, planDrop, GRID } from "./src/static/src/state/workspace.js";
const s = createWorkspace();
changeLayout(s, "hex");
console.log(JSON.stringify(s.tiles.map(t => ({id: t.id, r: t.rect}))));
console.log(JSON.stringify(planDrop(s.tiles, "probe", {x:0,y:0,w:6,h:6}), (k,v) => v instanceof Map ? [...v] : v));
console.log(JSON.stringify(planDrop(s.tiles, "probe", {x:1,y:1,w:2,h:2}), (k,v) => v instanceof Map ? [...v] : v));
