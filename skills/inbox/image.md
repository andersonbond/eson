---
name: inbox-image
description: Triage an image from inbox into a fixed report under exports/reports/
enabled: true
inbox_ext: png,jpg,jpeg,gif,webp
---

You are running a **background turn** triggered by a new image in
`workspace/inbox/`. The dispatch user message contains the workspace-relative
path and basename. The orchestrator will move the source to
`inbox/processed/<today>/` (or `inbox/failed/<today>/` on error) and append a
one-liner to today's digest at `exports/reports/digest-<today>.md` —
**do not move, delete, or write the digest yourself**.

## Steps

1. Call **analyze_visual** on the source path with the question:
   _"Describe the image content, transcribe any visible text verbatim, and
   identify what category this image belongs to (receipt, screenshot, photo,
   diagram, document scan, chart, ID, other)."_

2. **Classify** into exactly one of: `receipt`, `screenshot`, `photo`,
   `diagram`, `document-scan`, `chart`, `id`, `other`.

3. **workspace_write** a Markdown report to `exports/reports/<basename>.md`:

   ```markdown
   # <basename>

   - **source**: `inbox/<original-name>`
   - **classification**: `<receipt|screenshot|photo|diagram|document-scan|chart|id|other>`
   - **processed at**: <ISO timestamp>

   ## Description
   <2–4 sentences of what's in the image>

   ## Extracted text
   ```
   <verbatim OCR text, or _n/a_ if none>
   ```

   ## Key facts
   - <category-appropriate facts; for receipts: merchant, date, total, currency,
     items; for screenshots: app, action shown; for diagrams: concepts depicted;
     for photos: subjects, location cues; for IDs: type, issuing authority,
     redact numbers — say "<id-number redacted>">

   ## Suggested next actions
   - <2–3 concrete bullets, e.g. "log to expenses ledger", "file under
     docs/receipts/2026-04/", "extract diagram into structured notes">
   ```

4. **For receipts and IDs only**, also call **store_memory** with a short topic
   and a one-line summary so the durable fact is searchable later (e.g.
   topic `expense.2026-04`, body `2026-04-19 Pancake House ₱471.90`). Skip
   `store_memory` for transient screenshots and casual photos.

5. If you noticed a recurring source / format worth re-using (e.g. "Pancake
   House receipts split items into 'MYO TACO SERVING' lines with the price two
   columns to the right"), call **record_learning** with `kind="lrn"`, tags
   including `inbox-image` and the classification.

6. Reply with **one short line**:
   `processed <basename> → <classification> · report=exports/reports/<basename>.md`.
