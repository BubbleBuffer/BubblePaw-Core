# Workbench design

Status: visual specification for the local web workbench, moved here from the
top-level design file on 2026-10-21. The workbench frontend is not part of this
repository. It is a quiet local conversation surface in the BubbleBuffer family;
product goals are in [workbench product brief](product.md).

## Tokens

```yaml
colors:
  milk-cloth: "#F3EFE7"
  raised-milk: "#FFFAF1"
  muted-cloth: "#DCD4C8"
  duvet-ink: "#282A2F"
  muted-taupe: "#625C55"
  rail-muted: "#554F49"
  soft-thread: "#B8ADA1"
  strong-thread: "#6C645D"
  calico-apricot: "#DE945F"
  duvet: "#2F3138"
  dark-surface: "#30333A"
  dark-surface-muted: "#3B3E45"
  muted-cream: "#C6BAAC"
  dark-thread: "#575B64"
  dark-thread-strong: "#999DA6"
  bright-apricot: "#F0A064"
  focus-light: "#407ed1"
  focus-dark: "#7AB7FF"
  danger-light: "#874039"
  danger-dark: "#E5B1AA"
typography:
  brand:
    fontFamily: "Bricolage Grotesque Variable, sans-serif"
    fontSize: "21px"
    fontWeight: 650
    lineHeight: 1.2
    letterSpacing: "-0.025em"
  assistant:
    fontFamily: "Atkinson Hyperlegible Next Variable, sans-serif"
    fontSize: "17px"
    fontWeight: 400
    lineHeight: 1.6
    letterSpacing: "normal"
  user:
    fontFamily: "Atkinson Hyperlegible Next Variable, sans-serif"
    fontSize: "15px"
    fontWeight: 400
    lineHeight: 1.5
    letterSpacing: "normal"
  code:
    fontFamily: "Fragment Mono, monospace"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.7
    letterSpacing: "normal"
rounded:
  control: "7px 11px 6px 9px"
  field: "12px 18px 10px 16px"
spacing:
  small: "8px"
  medium: "16px"
  large: "24px"
  message: "32px"
components:
  send-button:
    backgroundColor: "{colors.calico-apricot}"
    textColor: "{colors.duvet-ink}"
    rounded: "{rounded.control}"
    padding: "10px 14px"
    height: "44px"
```

## Overview

The workbench is a basic frontend around the Rust harness. Saved chats, readable
messages, tool results and the next input are its entire working surface.
The server supplies runtime facts. Product styling must not add management panels
or imply a broader agent platform.

## Colors

Milk owns the light work surface; a muted cloth session rail provides orientation.
Apricot is reserved for sending and live attention. One simple appearance is used.
Connection, run and error states
always have words. Focus blue has its own role separate from the primary action.
Sidebar supporting text uses the stronger rail-muted role against muted cloth.

## Typography

Self-hosted Bricolage supplies the small product identity and empty-state heading.
Atkinson Hyperlegible Next carries conversation, controls and state. Assistant prose
has a comfortable 17px size and 1.6 line-height; user messages are compact at 15px.
On phones assistant prose reduces to 16px. Fragment Mono is reserved for tool payloads
and code. The conversation's 78ch outer measure includes padding, yielding roughly
65–75ch for comfortable assistant text on wide screens.

## Layout

A 52px utility band sits above a 240px session rail and conversation. Session title
and factual launch context remain small. The transcript scrolls independently;
the composer and recovery controls remain reachable. At 800px the rail narrows;
at 600px it becomes a compact horizontal session selector with a visible new-session
action and a 48px utility band. Mobile preserves conversation width without a generic
navigation drawer.
The root grid and nested work regions have zero minimum width; long code and tool
payloads scroll within their own focusable blocks rather than widening the page.

## Elevation & Depth

The surface is flat. Tonal fields and single thread boundaries establish context.
Selected sessions use an inset outline. No decorative shadows or glass effects.

## Shapes

Controls use simple small corners and ordinary boundaries. Avoid decorative
message cards, oversized draft containers and repeated product ornaments.

## Components

The send action owns the apricot fill. Stop is separate and names cancellation.
During work, an explicit selector chooses steering or a queued followup and explains
when it applies. Saved calls and results expand in place with native disclosure.
Pending inputs and resume sit beside the composer. Errors preserve drafts and offer
a narrow refresh action. Buttons retain loading labels and disabled semantics.
Failed history loads retain the selected session title and its draft, and show
retry/new-session recovery. Sending and continuation remain disabled until a
successful history retry. The transcript and code/tool blocks are keyboard-focusable
scroll regions with a visible blue focus outline.
Browser selection, caret, scrollbars, focus and reduced motion remain usable.

CodeMode outputs use Markdown, explicit file links and raster images. Existing
archived table data remains readable without a table publication workflow.
Environment management is a backend capability, not a panel in this frontend.
Basic Stop remains available for active runs and background cells.
Tool disclosures show JS and readable outcomes first, with raw payloads behind
an explicit disclosure. Errors state the concrete cause and available correction.

## Do's and Don'ts

- Keep compact user input and spacious assistant output distinct.
- Keep action evidence available without an operations console.
- Show real launch configuration and explicitly label the demo fixture.
- Preserve drafts on errors and selection changes; browser storage carries them
  through reloads when available.
- Avoid dashboard card walls, oversized mastheads, invented model capabilities,
  decorative metadata, attachment controls and sign-in flows.
