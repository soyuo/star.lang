# Star Online-Judge Fixture Pack

This directory publishes deterministic sample cases for solutions driven by stdin and stdout. `manifest.toml` is the machine-readable case index. Inputs are complete multi-case streams; outputs are compared byte-for-byte, including line endings and the final newline.

## Problems

| Case | Category | Required behavior |
|---|---|---|
| `array-sum` | Arrays and integers | Sum each case's array; include negative and arbitrary-precision values. |
| `map-distinct` | Hash maps | Count distinct string keys while preserving correct duplicate handling. |
| `graph-bfs` | Graphs | Report unweighted shortest-path distance from a source in an undirected graph; use `-1` when unreachable. |
| `fibonacci-dp` | Dynamic programming | Compute Fibonacci values for several inputs, including a value larger than 64-bit range. |
| `sort-search` | Sorting and searching | Sort each list, then report the lower and upper bound of the target. |
| `string-lengths` | Strings | Report Unicode-scalar length and UTF-8 byte length, including empty and multibyte strings. |

Each algorithm input contains multiple cases in one process. Numeric and string values are on separate lines; the string fixture deliberately contains empty and whitespace-bearing lines, and the Fibonacci stream ends without a final line terminator. The shared zero-byte empty-input case is applied to every solution pair. This keeps the pack usable with the current line-oriented `INPUT` instruction while exercising whitespace and EOF handling.

## Running a solution

The pack contains problem fixtures, not reference solutions. Provide a directory containing one paired source and bytecode solution per problem, named `<case-id>.star` and `<case-id>.starc`. From the repository root, run the parity harness with Python 3.11 or newer:

```sh
python tests/oj/run.py --solutions path/to/solutions --star-cli target/release/star
```

The harness applies every manifest input to both files with identical `star run` limits, then compares each exit code and stdout against the expected fixture and against the other engine. It prints the per-engine elapsed time, peak guest-memory estimate, and step count. `--star-cli` defaults to `star` from `PATH`. Diagnostics and statistics are captured separately from judged stdout.

## Measurement record

For each published measurement, record the case ID, Star revision, Rust version, target triple, engine, exit code, execution time, and peak guest-memory estimate printed by the harness. State the hardware and method for any language-to-language baseline. The runtime estimate is tracked guest allocation, not process RSS. The fixture data itself is deterministic; timing and memory measurements are platform-specific.
