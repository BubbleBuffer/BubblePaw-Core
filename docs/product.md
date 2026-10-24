# Workbench product brief

Status: product brief for the local web workbench, moved here from the top-level
product file on 2026-10-21. The workbench frontend is not part of this repository;
the Rust web host (`--features web`) and its [contract](web-contract.md) are.
Operating context (loopback server embedding the harness, SQLite canonical history,
disposable in-memory serving view) is described in the
[README](../README.md#local-web-workbench).

## Users

The BubblePaw owner, working locally with a coding agent and its tools.

## Purpose

Provide a simple web interface to the standalone Rust harness's basic capabilities.
Make a real conversation, tool use, interruption and saved-session continuation
usable before adding product orchestration or advanced context handling.
Workspace, provider, model and tool access are selected when launching.

## Constraints

File attachments are referenced in the workspace, but images and binary files are
not native model input. There is no provider sign-in flow or server deployment.
The runner builds [child sessions](child-sessions.md); product orchestration stays
outside the engine.

## Brand commitments

BubblePaw's quiet workbench dialect from the BubbleBuffer design system: warm
working fields, apricot action emphasis, compact user messages, spacious assistant
content and relevant tool details. Use the established family, not a new identity.
The visual specification is in [workbench design](design.md).

## Principles

- Keep the conversation and next useful action prominent.
- Keep harness persistence and scheduling authoritative.
- Preserve drafts and saved history through errors and reconnects.
- Display actual capability and connection state.
- Keep advanced machinery out of the first usable interface.
