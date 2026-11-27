Perform a consistent refactor across all TypeScript modules and tests in this workspace:

1. Rename the type `AccountId` to `LedgerAccountId` everywhere it is declared, imported, exported, or referenced.
2. Rename the field/identifier `amountCents` to `amountMinor` everywhere in TypeScript code and tests.
3. **Important wire-format constraint**: In `src/legacy-wire.ts`, the external v1 JSON wire key `"amount_cents"` MUST remain `"amount_cents"` when serializing and parsing JSON.

Verify with `bun test`.
