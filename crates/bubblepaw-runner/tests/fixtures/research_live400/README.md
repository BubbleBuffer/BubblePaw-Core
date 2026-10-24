# Recorded OpenCode Messages research child

Recorded on Linux on 2026-10-14, using only synthetic fixture content.
Provider: opencode-messages; model: claude-haiku-5-5; effort: low;
max_tokens: 16384; output_schema: findings/source_ids/gaps/usable;
max_tool_calls: 6. A loopback backend serves one search and one fetched page.
Repeated cached calls exhaust six slots across two sequential JS tool rounds.

The three success responses and requests come from live capture
`7dd0f635-ec28-4f91-8fbf-223f2f3d85ab` (global requests 6, 7, 8).
Bodies are the complete original SSE bytes. Capture redaction finds no credential
echo, so it makes no replacements. Request files contain JSON only: no headers,
authentication values or private task content. Git attributes preserve raw bytes.
The local ports and generated identifiers are synthetic/disposable.

The first response has real signed thinking before its JS tool_use; displayed
thinking is empty but its signature is complete. The second response calls JS
again, and the third returns final schema JSON with tools disabled. The test
reconstructs all original SSE blocks independently and compares the full ordered
assistant content, including signature bytes, against every follow-up. It also
checks stable system/tool declarations and unchanged prior messages. The fake
transport checks these values without sending recorded signatures to a live API.

`failure/` retains the original failing pair from capture
`e34129c4-fcac-4638-856a-220ec4a8c678` (global requests 4, 5). Its thinking block
and 1760-byte signature replay identically, but the system countdown changes
from 6 to 3. The retained 400 body identifies this conversation-prefix mismatch.
These files are evidence, not success replies in the offline replay sequence.

Response SHA-256:

- `response-001.body`: `b5e277f1581ffdc49cc627727653d1f1250851d484b6265c4b268f2590478c7d`
- `response-002.body`: `273408dabea17bbd51806504c48e732dabe3b9fa6022d93ab7588a4783e2d49d`
- `response-003.body`: `20bba24647c06e9b3a1b5124f9e829caedbc20765b5c0750e0f96097de624a7b`
