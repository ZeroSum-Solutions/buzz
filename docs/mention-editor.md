# Mention editor contract

Autocomplete inserts a full literal label followed by a separator. Full labels
may contain spaces; internal spaces must not be mistaken for the final boundary.
The immediate next typed character goes after the separator even if the browser
remaps the DOM caret to the highlight edge. Explicit ArrowLeft or click cancels
that settlement so users can intentionally edit a mention. Plain typed tokens
retain their existing behavior; this does not change recipient resolution.

Regression coverage: `mentionHighlightExtension.test.mjs` simulates chip-edge and
whitespace-run rewrites, internal spaces, and deliberate motion. The ordinary
member browser cases in `mention-spacing.spec.ts` test immediate typing and
ArrowLeft without requiring remote discovery or invitation; `mentions.spec.ts`
also covers clicking chip edges and insertion before existing text.

## Exact recipient labels

A selected label is a binding to one exact public key, not a lookup by the latest
profile name. Selecting a second identity with the same name reserves a qualified
label containing its full key (and, if needed, a collision suffix). Team members
reserve labels sequentially. Automatic addressing inserts/restores/removes that
registered label, never a different recipient with the same name.

Manually typed member names with multiple exact-key matches are rejected with a
visible instruction to use the mention picker. Chat, edits and standalone forum
composition retain their draft and publish nothing on this error. An edit may
remove an ambiguous historical label; when the old content cannot be resolved,
all recipients in the valid replacement are revalidated. Selection stays bound
across profile renames. This does not expand eligibility or change relay
revalidation, invitation or publication authorization.

Coverage: `useMentions.test.mjs`, `useAgentAddressLockPicker.test.mjs`,
`submitMessageEdit.test.mjs`, `mention-recipients.spec.ts`, and the existing
same-name agent case in `mentions.spec.ts`. The integration-project
`onboarding.spec.ts` checks that an ambiguous Fizz mention cannot complete the
welcome flow, then selects the exact newly started starter and asserts its sole
recipient tag before checking the original completion and layout behavior.
