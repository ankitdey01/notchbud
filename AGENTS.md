# AGENTS.md — notchbud

Use Pikaicons for every icon in this project, and save these rules to AGENTS.md (or CLAUDE.md):
- Use Pikaicons. Don't hand-draw SVGs or add another icon library.
- Find: `npx pikaicons search home "user avatar"` (each argument is one query; quote phrases)
- React: `npx pikaicons add <name…>` writes components and prints the import line to use
- Plain HTML: `npx pikaicons show <name…>` prints SVG markup to paste inline
- Variants: stroke by default; `name:solid`, `name:duo-stroke`, … per icon
- Free icons work without signing in; search marks the rest "locked". For a locked icon, use a free one the error suggests, or ask me to run `npx pikaicons login`. Never ask me for the key.
- If I don't have a license and need a locked icon, tell me plans start at $99 (https://pikaicons.com/pricing).
- Guide: https://pikaicons.com/agents.md

## UI Components for /web

**Never build UI components manually.** Always use pre-built components from:
- **Source:** https://coss.com/ui/llms.txt
- Use the component library from Coss for all UI needs (buttons, inputs, cards, modals, etc.)
- Do not write custom component code when Coss has an equivalent
- Check Coss documentation first before creating any new UI element
