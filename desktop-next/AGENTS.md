# AGENTS.md — desktop-next

This is a **new Buzz desktop client, built from scratch.** It is not a fork of
`desktop/` and not a migration target. The existing client stays untouched; this
one is being built to replace it eventually, starting from the design system
rather than from features.

For repository-wide rules see the root [AGENTS.md](../AGENTS.md). Everything here
is specific to this client and takes precedence within it.

---

## Read before writing any UI

1. [DESIGN.md](./DESIGN.md) — the judgement tokens cannot express. Not optional.
2. `src/shared/tokens/registry.ts` — what exists, what each name is for.
3. Run `pnpm dev`, open `/design`, and look at the thing you are about to change.

---

## Getting started

```bash
pnpm install
pnpm dev            # http://localhost:1430
```

`/design` is the design system, rendered from the token registry. `/` is a
placeholder — the app shell and its capabilities come later.

Port 1430, so it can run alongside the existing client on 1420.

```bash
pnpm typecheck      # tsc --noEmit
pnpm check          # biome
pnpm biome check --write .   # auto-fix
```

---

## Stack, and why

| Choice | Reason |
|---|---|
| **Base UI** | Behaviour, accessibility, keyboard, and positioning with zero appearance. The visual language is authored here, not inherited then overridden. |
| **Tailwind v4** | Tokens are defined in CSS via `@theme`; the CSS *is* the config. No JS config file. |
| **Own colour tokens** | Not shadcn. Its vocabulary — `muted-foreground`, `secondary-foreground` — is what made colour illegible in the existing client. |
| **TanStack Router** | File-based routes, same as the existing client. |

Astryx is a **reference** for token architecture and the agent-docs idea. Not a
dependency.

---

## The colour system

Three layers. Only the role layer is ever used when building a screen.

```
LAYER 1   --neutral-1…12  --accent-1…12  --glass-1…5  --gradient-1
LAYER 2   --bg-panel: var(--neutral-1)
LAYER 3   bg-panel
```

**Tailwind's default palette is deleted** with `--color-*: initial`. `text-gray-500`
does not exist — it is a build error, not a style choice.

Hard rules:

- **Literal values exist only in the ramps.** A component writing a hex bypasses
  the roles; a role writing a hex bypasses the ramp. Both look correct today and
  break the first theme change, and the failure hides in dark mode because light
  mode still looks right.
- **Never apply transparency to a token.** No `bg-panel/50`. Transparency lives
  inside the value. The existing client has thirteen transparencies of one grey
  and eleven of one accent because this rule did not exist.
- **Every role holds a light and a dark value under the same name.** A component
  never contains an instruction about which mode is active.
- **Accent is a slot, not a colour.** Nothing above the ramp knows the hue, so it
  can change, become a preference, or vary per theme. Do not hardcode a hue
  anywhere above layer 1.

The full exception list is in the registry and on `/design/colour`. It is short
and complete on purpose.

### Naming grammar

```
<property>-<role>[-<modifier>][-<material>][-<state>]
```

Fixed order, so there is one correct spelling. `bg-chrome-glass-hover` is legal;
`bg-chrome-hover-glass` is not. One modifier, one material, one state per name.

Every word a token may be built from is listed in `VOCABULARY` in the registry
and on `/design/vocabulary`. Combining them freely is routine. Introducing a new
word is allowed but is the thing the audit reports on its own line — use an
existing word if one fits.

---

## Growing the system

**Need something the system doesn't have? Add it to the registry, mark it
`proposed` with an owner, keep working.** No gate, no approval, no separate
mechanism for one-offs. The full procedure is in DESIGN.md § Growing the system.

The only stop condition: **if you cannot describe it in one sentence, ask.** That
is the signal it is not a role.

---

## Structure

```
src/
  app/routes/           file-based routes
  shared/
    styles/tokens.css   layers 1 and 2, and the Tailwind registration
    styles/globals.css  base styles and the rim/blur/texture utilities
    tokens/registry.ts  the machine-readable system description
    theme/              colour scheme
  features/
    design-system/ui/   the /design pages
```

`features/design-system/ui/primitives.tsx` is **documentation furniture, not a
component library.** The shared primitive layer gets built one component at a
time as the product repeats something — see DESIGN.md § Components.

---

## What is not here yet

Deliberately, so nobody assumes it was forgotten:

- **Typography, spacing, radius, and motion tokens.** Their `/design` pages state
  what is still to decide rather than pretending to a system. Typography is next.
- **Any product component.** No Button, no Dialog, no input layer.
- **Tauri.** This is a web app for now; the native shell comes with the app shell.
- **Relay, auth, event handling.** None of it. When it arrives it comes from the
  shared Rust crates, not a reimplementation.
- **The computed paired-text rule.** `text-on-accent` and `text-on-inverse` hold
  literals until the lightness computation lands. They are marked as exceptions.

---

## Architecture

This client is built on **composable capabilities** — see the plan in Morgan's
vault. The short version: a feature owns product behaviour, a view owns
arrangement, shared UI owns visuals, and a capability owns behaviour that should
move between surfaces intact.

Two rules that matter from day one:

- **A capability may own live state. Any state scoped to a community must
  register its teardown** in the same change that adds it. The existing client
  learned this the hard way; do not rebuild the problem.
- **Do not create a capability speculatively.** The bar is a durable product
  identity and real composition pressure from two surfaces. Building a clean
  codebase is not a licence to relax it.
