// views/providers/custom-model.ts — custom-model hook of the provider
// detail page.
//
// Split out of the former detail.ts monolith (FU1). The custom-model
// form itself (modal, validation, submit) already lives in
// components/model-custom-form.ts — there is no separate
// renderCustomModelForm in the detail view; the "Custom model" button
// rendered by models.ts binds to the adapter exported here.

import { showCustomModelForm } from '../../components/model-custom-form.js';

export function onShowCustomModelForm(providerId: string): void {
  showCustomModelForm(providerId);
}
