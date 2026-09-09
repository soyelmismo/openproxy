// lib/mutate.ts — `mutateAndRefresh`: centralise the trivial
// "POST/PATCH/DELETE + toast + re-render" pattern that recurs across
// handlers. Errors funnel through `showApiError`; the helper never
// throws.
//
// The handler is OPT-IN: it is meant for the call-sites that fit
// the Tier 1 / Tier 2 pattern in §3.Q10 of the refactor spec
// (single api call, optional refetch in `onSuccess`, final
// `requestUpdate()`). Handlers with multi-call flows, optimistic
// local mutations before the await, or focus-preserving in-place
// DOM patches should stay inline — they get noisier if forced
// through this helper.

import { showApiError } from "./ui-utils.js";
import { showToast } from "../components/toast.js";
import { requestUpdate } from "../state/reactive.js";

export interface MutateOptions<T> {
  /** The API call to perform. */
  apiCall: () => Promise<T>;
  /** Optional success toast. Omit for silent operations (e.g. toggles). */
  successMessage?: string;
  /** Prefix for the error toast via `showApiError`. Defaults to `"Error"`. */
  errorMessage?: string;
  /**
   * Optional post-success hook. Use this to re-fetch other
   * resources (e.g. after a toggle, refetch both the parent and
   * the child collection), or to apply optimistic local mutation
   * AFTER the server has acknowledged the change.
   * Called BEFORE `requestUpdate`.
   */
  onSuccess?: (result: T) => void | Promise<void>;
}

/**
 * Run `apiCall`, surface success/failure to the user, and schedule
 * a re-render. Returns `true` on success, `false` on error.
 * Never throws.
 */
export async function mutateAndRefresh<T>(options: MutateOptions<T>): Promise<boolean> {
  try {
    const result = await options.apiCall();
    if (options.onSuccess) {
      await options.onSuccess(result);
    }
    if (options.successMessage) {
      showToast(options.successMessage, "success");
    }
    requestUpdate();
    return true;
  } catch (err: unknown) {
    showApiError(err, options.errorMessage ?? "Error");
    return false;
  }
}
