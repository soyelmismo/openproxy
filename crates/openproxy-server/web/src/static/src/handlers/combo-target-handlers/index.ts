// handlers/combo-target-handlers/index.ts — facade re-exporting
// all combo target CRUD: modal, DnD, selection, bulk actions.
//
// Public API is identical to the original combo-target-handlers.ts.

export {
  showAddTarget,
  closeAddTarget,
  onTargetKindChange,
  onTargetModelSearch,
  onModelCheckboxChange,
  selectAllModelsInModal,
  deselectAllModelsInModal,
  addTarget,
} from "./add-target-modal.js";

export {
  initDragAndDrop,
} from "./drag-and-drop.js";

export {
  deleteTarget,
  resetCooldown,
  changePriority,
  updateTargetWeight,
  toggleTargetSelection,
  toggleSelectAllTargets,
  clearTargetSelection,
  bulkDeleteSelectedTargets,
} from "./target-operations.js";
