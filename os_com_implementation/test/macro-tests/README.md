# Combined Macro Harness

## Purpose

Runs the Filebench and SQLite harnesses serially with one shared parameter set and records a manifest linking their independently owned results.

## Commands

- `./run-macro-tests.sh --dry-run`: print both matrices without creating results.
- `./run-macro-tests.sh --confirm-destroy`: run both destructive device-backed suites.
- `./summarize-macro.py ../filebench-test/results/<run>/summary.csv ../sqlite-test/results/<run>/summary.csv`: render a comparison table.

## Output

The combined manifest is written to `results/<timestamp>_macro/`. Filebench and SQLite data remain in their respective test subdirectories.

## Safety

Non-dry runs format configured devices and require exclusive QEMU/device ownership.
