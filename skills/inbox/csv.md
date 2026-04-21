---
name: inbox-csv
description: Triage a CSV from inbox into a fixed report under exports/reports/
enabled: true
inbox_ext: csv
---

You are running a **background turn** triggered by a new CSV in `workspace/inbox/`.
The dispatch user message contains the workspace-relative path and basename. The
orchestrator will (1) move the source file to `inbox/processed/<today>/` (or
`inbox/failed/<today>/` on error) and (2) append a one-liner to today's digest at
`exports/reports/digest-<today>.md` automatically — **do not move, delete, or
write the digest yourself**. Focus on producing a useful report.

## Steps

1. **workspace_read** the source path with `max_bytes` ≈ 65_536 to inspect headers
   and a sample of rows.
2. **Classify** the dataset into exactly one of:
   - `financial` — money columns (amount, debit/credit, currency, balance,
     invoice/receipt numbers, dates).
   - `metrics` — numeric series with a time column (timestamp, date, hour) and
     dimensions (region, host, product).
   - `log` — timestamped event rows (level, message, request_id, status_code).
   - `inventory` — sku/code, quantity, location, price columns.
   - `other` — anything that doesn't fit the above (note your guess in the report).
   Decide from column names + value shapes.
3. **workspace_write** a Markdown report to `exports/reports/<basename>.md` with
   this exact skeleton (fill every section; if a section truly has nothing,
   write `_n/a_`):

   ```markdown
   # <basename>

   - **source**: `inbox/<original-name>`
   - **classification**: `<financial|metrics|log|inventory|other>`
   - **rows (sampled)**: <approx-row-count> · **columns**: <col-count>
   - **processed at**: <ISO timestamp from chat context>

   ## Schema
   - `<col_name>` — `<inferred type: int | float | string | date | bool | enum<…>>`
     · _<one-line note: range, cardinality, missing rate>_
   - …

   ## Highlights
   - <3–5 bullets: totals, ranges, outliers, missing-value rates, dominant categories>

   ## Suggested next actions
   - <2–3 concrete bullets the user could ask for next, e.g. "render bar chart of
     revenue by month", "join with customers.csv on cust_id", "filter rows where
     status=ERROR and group by service">
   ```

4. If the data clearly benefits from a chart (numeric series + categorical or
   temporal axis), call **render_chart** and write the HTML to
   `exports/charts/<basename>.html`. Mention the chart path in the
   "Suggested next actions" section of the report.

5. If you spotted a recurring pattern worth re-using on future drops (e.g. "BPI
   bank exports always have 4 metadata lines before the header row"), call
   **record_learning** with `kind="lrn"`, a one-sentence summary, full detail in
   `body`, and comma-separated `tags` (include `inbox-csv` and the
   classification, e.g. `inbox-csv,financial,bank-statement`).

6. Reply with **one short line** to the void:
   `processed <basename>.csv → <classification> · report=exports/reports/<basename>.md`.
