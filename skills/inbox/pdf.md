---
name: inbox-pdf
description: Triage a PDF from inbox into a fixed report under exports/reports/
enabled: true
inbox_ext: pdf
---

You are running a **background turn** triggered by a new `.pdf` in
`workspace/inbox/`. The dispatch user message contains the workspace-relative
path and basename. The orchestrator will move the source to
`inbox/processed/<today>/` (or `inbox/failed/<today>/` on error) and append a
one-liner to today's digest at `exports/reports/digest-<today>.md` —
**do not move, delete, or write the digest yourself**.

## Steps

1. Call **analyze_visual** on the source path with the question:
   _"Summarize the document. Extract: title, dates, parties/people/orgs, totals
   or amounts (with currency), section headings, and any tables or line items.
   If this looks like an invoice/receipt/contract/report, say so."_

2. **Classify** into exactly one of: `invoice`, `receipt`, `contract`,
   `report`, `article`, `letter`, `form`, `other`.

3. If the document contains structured tables (invoices, financial reports,
   purchase orders, line items), call **pdf_to_table** and write to
   `exports/tables/<basename>.csv` (or `.xlsx`). Skip this step for prose-heavy
   documents (articles, letters, contracts without numeric tables).

4. **workspace_write** a Markdown report to `exports/reports/<basename>.md`:

   ```markdown
   # <basename>

   - **source**: `inbox/<original-name>`
   - **classification**: `<invoice|receipt|contract|report|article|letter|form|other>`
   - **pages (analyzed)**: <n>
   - **processed at**: <ISO timestamp>
   - **table extract**: `exports/tables/<basename>.csv` _(or "n/a")_

   ## Key facts
   - **title**: …
   - **date(s)**: …
   - **parties / people / organizations**: …
   - **totals / amounts**: … (include currency)
   - **identifiers**: <invoice no., contract id, OR no., etc., or _n/a_>

   ## Section outline
   - <heading 1>
     - <subheading or one-line summary>
   - …

   ## Notable line items / tables
   - <2–4 bullets summarizing what's in the structured data, or _n/a_>

   ## Suggested next actions
   - <2–3 concrete bullets, e.g. "file under tasks/finance/2026-Q2/",
     "reconcile against bank statement", "remind me of due date 2026-05-15">
   ```

5. If you noticed a recurring vendor / format / heuristic worth re-using on
   future drops (e.g. "Globe receipts always put the total on the last
   non-empty line above 'TOTAL DUE'"), call **record_learning** with
   `kind="lrn"`, tags including `inbox-pdf` and the classification.

6. Reply with **one short line**:
   `processed <basename>.pdf → <classification> · report=exports/reports/<basename>.md`.
