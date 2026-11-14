// Injected by esbuild `define` (build.mjs from the OP_PAYMENTS env var):
// whether this bundle was built with the on-chain payments/credits feature.
// In a payments-disabled build the credits view chunk is never emitted and
// the checkout bundle is not built at all.
declare const __OP_PAYMENTS__: boolean;
