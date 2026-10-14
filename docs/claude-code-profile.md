# Claude Code profile synchronization

The runtime contract is a tuple, not two independent version tags. Stainless
must be measured from the same Claude binary as its User-Agent. npm's latest
`@anthropic-ai/sdk` does **not** identify the SDK embedded in Claude Code.

## Capture and refresh

```sh
python3 scripts/capture-claude-code-profile.py \
  --output "$HOME/.openproxy/claude-code-profile.json"
```

The capture launches the **already installed** `claude` CLI interactively with
`--effort high`, in a fresh temporary HOME and work directory, against an
isolated loopback mock. A fictitious key is used. It does not read the user's
Claude credentials, install packages, update Claude, forward prompts to
Anthropic, or save Authorization headers. Only a validated main request with
`thinking.display=updates` is accepted, excluding auxiliary title requests.

It writes CLI version, embedded SDK version, runtime version, common betas and
request defaults atomically. Model/payload-dependent betas remain derived in
the adapter from the final body after parameter stripping.

Openproxy's supervised `claude_code_profile` task invokes the capture at startup
and every six hours. Request paths read an in-memory snapshot and never launch
processes or access npm. A missing CLI, capture failure, malformed cache, or
version downgrade keeps the last known good in-memory profile. A valid
on-disk cache is loaded on restart. Without a valid cache the measured
2.1.293/0.128.0 contract is the fallback.

Updating the installed CLI through its normal update mechanism allows the
next capture to adopt the measured new tuple, betas and supported request
shapes without recompiling openproxy. Unsupported shapes are rejected rather
than guessing how to use them. Model capability gates still require code
updates for genuinely new model families.

Environment controls:

- `OPENPROXY_CLAUDE_CODE_PROFILE_PATH`: cache JSON path, default
  `$HOME/.openproxy/claude-code-profile.json`
- `OPENPROXY_CLAUDE_CODE_PROFILE_CAPTURE_SCRIPT`: capture script path, default
  the repository script
- `OPENPROXY_CLAUDE_CODE_PROFILE_REFRESH_SECS`: refresh interval, default 21600,
  minimum 300
- `OPENPROXY_CLAUDE_CODE_PROFILE_AUTO_REFRESH=false`: disable capture service
- Existing `OPENPROXY_CLAUDE_CODE_*_VERSION`/header overrides remain manual
  overrides. Keep their CLI, SDK and runtime tuple coherent

## Reference audit

The inspected CLIProxyAPI checkout uses configurable header defaults,
per-credential/subclient profile stabilization, seven-day sliding TTL and
baseline normalization (`claude_device_profile.go`). Its candidate profile
must exactly match the configured software baseline before storage. This is
not an npm/release synchronization mechanism. Its `ClaudeDeviceProfileBeforeCandidateStore`
hook is not proof of release fetching.

Captures of Claude 2.1.293 main interactive requests show SDK 0.128.0,
`thinking={type:adaptive,display:updates}`, `output_config={effort:high}`,
`context_management.edits=[{type:clear_thinking_20251015,keep:all}]` and
one-hour ephemeral cache breakpoints. Auxiliary structured-output requests
have different thinking/betas and must not replace the main profile.
