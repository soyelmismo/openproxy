// handlers/provider-handlers/index.ts — facade.
//
// Re-exports the public API from sub-modules so consumers
// can `import { ... } from "./provider-handlers.js"` unchanged.
// The original monolith was split into:
//   list.ts  — provider CRUD, refresh, rename, bulk toggle
//   detail.ts — headers editor, account health/quota

export {
  refreshProvider,
  refreshAllProviders,
  showCreateProvider,
  closeCreateProvider,
  createProvider,
  deleteProvider,
  confirmDeleteProvider,
  toggleProviderActive,
  renameProviderPrompt,
  editProviderEndpointPrompt,
  bulkToggleModels,
} from "./list.js";

export {
  editProviderHeadersPrompt,
  setHealth,
  refreshAccountQuota,
  refreshAllQuotas,
} from "./detail.js";
