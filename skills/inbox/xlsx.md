---
name: inbox-xlsx
description: Triage an Excel workbook from inbox into a fixed report under exports/reports/
enabled: true
inbox_ext: xlsx
---

You are running a **background turn** triggered by a new `.xlsx` in
`workspace/inbox/`. The dispatch user message contains the workspace-relative
path and basename. The orchestrator will move the source to
`inbox/processed/<today>/` (or `inbox/failed/<today>/` on error) and append a
one-liner to today's digest at `exports/reports/digest-<today>.md` —
**do not move, delete, or write the digest yourself**.

## Steps

1. Convert the workbook to per-sheet CSVs under `exports/tables/<basename>/`
   using **run_terminal**. Prefer a tiny Python one-liner if Python is
   available, fall back to `ssconvert` (gnumeric):

   ```bash
   python3 -c "import openpyxl, csv, os, sys
   src = sys.argv[1]; out = sys.argv[2]
   os.makedirs(out, exist_ok=True)
   wb = openpyxl.load_workbook(src, read_only=True, data_only=True)
   for s in wb.sheetnames:
       with open(os.path.join(out, s + '.csv'), 'w', newline='') as f:
           w = csv.writer(f)
           for row in wb[s].iter_rows(values_only=True):
               w.writerow(['' if v is None else v for v in row])" \
       <ABS_SRC> <ABS_OUT>
   ```

   If neither Python nor `ssconvert` is available, write the report with a
   `note` row explaining that conversion was skipped, and continue to step 4.

2. **workspace_read** the first one or two emitted CSVs (`max_bytes` ≈ 65_536)
   to sample headers and rows.

3. **Classify** the workbook into exactly one of: `financial`, `metrics`,
   `inventory`, `report`, `other`. Rule of thumb: a single sheet with money
   columns → `financial`; multi-sheet pivot/report → `report`.

4. **workspace_write** a Markdown report to `exports/reports/<basename>.md`:

   ```markdown
   # <basename>

   - **source**: `inbox/<original-name>`
   - **classification**: `<financial|metrics|inventory|report|other>`
   - **sheets**: <count> · **converted to**: `exports/tables/<basename>/`
   - **processed at**: <ISO timestamp>

   ## Sheets
   ### <sheet-name>
   - rows: <n> · cols: <n>
   - schema: `<col>` (`<type>`), …
   - highlights: <2–3 bullets>

   _(repeat per sheet, cap at the 5 most interesting if the workbook is huge)_

   ## Suggested next actions
   - <2–3 concrete next steps, e.g. "render chart from <sheet>.csv",
     "join sheet A with sheet B on …">
   ```

5. If there's a clear chart-worthy series in any sheet, call **render_chart** to
   `exports/charts/<basename>-<sheet>.html` and reference it in the report.

6. If a reusable pattern stands out (e.g. "Q-by-Q financial reports always have
   a totals row at the bottom of each sheet"), call **record_learning** with
   `kind="lrn"`, tags including `inbox-xlsx` and the classification.

7. Reply with **one short line**:
   `processed <basename>.xlsx → <classification> · sheets=<n> · report=exports/reports/<basename>.md`.
