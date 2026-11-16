# Front-end direction: one app, production-grade live UI

Status: direction note, not scheduled. Owner reworks the UI later; this records the target so that work starts from a decision, not a survey.

## Goal

- Merge the admin cockpit (`/cockpit`) and the trader app (`/trade`) into one app.
- Live updates at production grade: blotters, positions, order book and charts ticking without jank.
- Interactive components: price charts, depth/order book, large grids.
- Look and feel close to Linear and Open WebUI: dense, neutral greys, dark-first, keyboard-first, a ⌘K command menu.

## Decision

Stay on **React + TypeScript + Vite**. Replace **Mantine** with **shadcn/ui (Radix primitives) + Tailwind**.

Why React over Svelte (Open WebUI) or others:
- The trading component ecosystem (data grids, charting, order-book examples) is React-first; Svelte equivalents are thin.
- Linear is built on React. shadcn/ui gives the Linear look with little custom design; Open WebUI's look is easy to reproduce with Tailwind.
- Largest hiring and handover pool.
- Current TanStack Query, React Router, API client and session code carry over unchanged. Only the component library changes.

## Stack

| Need | Choice |
|---|---|
| Components and look | shadcn/ui + Radix + Tailwind; `cmdk` for the ⌘K menu |
| Blotters, positions (thousands of ticking rows) | AG Grid (Community; Enterprise only if grouping/pivot is needed) |
| Price charts | TradingView Lightweight Charts (canvas, free) |
| Order book / depth | Custom component, virtualised or canvas-drawn. No library does this well. |
| Other charts (P&L, exposure) | ECharts or visx |
| Live state | Zustand stores fed by the socket; TanStack Query for REST only |
| Routing | React Router (existing) |
| Build | Vite, still embedded in the `oms` binary via `include_dir!` |

## Live-update rules

These matter more than the framework:

1. Ticking data never goes through React state per message. Socket messages write to a store or straight into AG Grid transactions (`applyTransactionAsync`), and rendering happens at most once per animation frame.
2. One WebSocket per session, with topic subscriptions (`orders`, `fills`, `positions`, `book:<symbol>`). The existing scoped sockets (scope reload on group change, close on revocation) already provide most of this.
3. Every stream carries a sequence number. On a gap or reconnect the client refetches a snapshot over REST and replays from there.
4. The UI shows stream health (connected, stale, reconnecting) at all times. A stale price must never look live.

## One app

- A single shell. Admin pages are permission-gated routes, hidden for non-admins. There is no separate cockpit.
- One sign-in page and one session. This also removes the current trap where a trader session shows "no access" in the cockpit.
- Navigation is a left sidebar in the Linear style, plus the ⌘K menu for every action and page.

## Estimate

- Merge the two apps and move the existing screens to shadcn/ui: about 2–3 weeks.
- Order book: about 1 week. Charts: about 1 week.

## Out of scope here

Visual design details (colours, typography, spacing tokens). Decide those at the start of the rework.
