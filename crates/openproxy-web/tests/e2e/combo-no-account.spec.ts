// e2e/combo-no-account.spec.ts — Adversarial test for the pre-walk
// filter in pipeline.rs that drops combo targets whose provider
// requires auth and the target has no account_id (account_id=null
// would have triggered a confusing `status=0` + "combo_target X has
// no account_id after expansion" error at the client).
//
// The user-visible contract: a combo whose only targets all point
// at auth-required providers with no accounts must return a clean
// 502 NoHealthyTargets (or a 200 from a healthy target if one
// exists in the same combo). In neither case should the client see
// a status=0 sentinel.
//
// Implementation status: SKIPPED.
//
// Why skipped: setting up the fixture requires creating a provider
// with auth_type=Bearer, a model row, a combo, a combo_target with
// account_id=None, and at least one chat completion through the
// admin API endpoints. The other e2e specs in this project only
// exercise the *read* side of the API (Live Logs, log detail
// modal). The admin write surface is large enough that wiring a
// full e2e fixture here is out of scope for this surgical fix.
//
// Coverage of the pre-walk filter is provided by:
//   1. The Rust unit test in
//      `crates/openproxy-core/src/pipeline.rs` (the new test that
//      exercises the filter against a fresh DB).
//   2. Manual `curl` validation: create a Bearer provider with no
//      accounts, a combo with one target pointing at it, make a
//      chat request, observe the response (clean 502, no status=0).
//
// TODO(operator): when the project's e2e suite grows a proper
// admin-fixture helper (see live-logs.spec.ts for the read pattern),
// un-skip this test and fill in the admin-API calls.
//
// @see tsconfig.test.json for type settings.

import { test } from '@playwright/test';

test.describe('Combo with targetless account_id (Bug 2) — user-visible contract', () => {
  test.skip('l) Combo with no account_id on a Bearer provider returns a clean 502, not status=0', async () => {
    // Implementation: see the TODO at the top of this file. The
    // pre-walk filter is covered by the Rust unit test; the e2e
    // here is the user-visible contract. Un-skip and wire admin
    // API calls once the fixture helper exists.
    //
    // 1. POST /v1/admin/providers (auth_type=bearer, 0 accounts)
    // 2. POST /v1/admin/models (a single model row)
    // 3. POST /v1/admin/combos (priority, 1 target)
    // 4. POST /v1/admin/combo_targets (account_id=null)
    // 5. POST /v1/chat/completions through the combo
    // 6. Assert: status_code !== 0; body carries either a 2xx
    //    response OR a clean 502 NoHealthyTargets error.
  });
});
