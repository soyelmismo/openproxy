
import { createWorkspace, changeLayout, planDrop } from "./state/workspace.js";
const s = createWorkspace();
changeLayout(s, "hex");
console.log("TILES", JSON.stringify(s.tiles.map(t => t.rect)));
console.log("PLAN1", JSON.stringify(planDrop(s.tiles, "probe", {x:0,y:0,w:6,h:6}), (k,v) => v instanceof Map ? [...v] : v));
console.log("PLAN2", JSON.stringify(planDrop(s.tiles, "probe", {x:1,y:1,w:2,h:2}), (k,v) => v instanceof Map ? [...v] : v));
